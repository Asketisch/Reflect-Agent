//! `TaskCreate` — 在指定 list(默认 session_id)创建一个结构化任务。
//!
//! 参数:`subject`(必填,短标题)、`description`(任务正文)、`activeForm`
//! (进行中显示短句,如 "Writing tests")、`owner`(agent id)、`metadata`
//! (任意 JSON)、`listId`(默认 ctx.session_id)。
//!
//! 返回:`text = "Task #<id> created: <subject>"`,metadata.task = 完整 task 对象。
//!
//! 副作用:`TaskManager::create_task` 自动分配 `output_path = <list>/<id>.output.md`
//! 并触发 `HookEvent::TaskCreated`(success path),Phase 1 钩子集成。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::tools::{list_id_from_args_or_ctx, parse_optional_string, parse_string_arg};

/// `TaskCreate` 工具实现。
///
/// `manager: Arc<TaskManager>` 持有共享状态;`execute` 委托给 manager 的
/// `create_task`,由 manager 统一处理 id 分配、持久化、钩子触发。
pub struct TaskCreateTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskCreateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskCreateTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskCreateTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "在当前会话(或指定 listId)的任务列表里创建一条结构化任务。\
         返回任务 id 与 subject,并自动分配 output_path。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Brief task title shown in lists"
                },
                "description": {
                    "type": "string",
                    "description": "What needs to be done (free-form multi-line)"
                },
                "activeForm": {
                    "type": "string",
                    "description": "Present-continuous form shown in spinners (e.g. 'Writing tests')"
                },
                "owner": {
                    "type": "string",
                    "description": "Agent id that will work on this task"
                },
                "metadata": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Arbitrary structured metadata attached to the task"
                },
                "listId": {
                    "type": "string",
                    "description": "List id; defaults to current session id"
                }
            },
            "required": ["subject"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // TaskManager 内部 next_id + save 在 list-level Mutex 下串行化,
        // 跨 list 互不影响 —— 多次并发调用各自拿到不同 id。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        // 创建任务是常规工作流操作,不需要 per-call 审批。
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let subject = parse_string_arg(&args, "subject")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let active_form = parse_optional_string(&args, "activeForm");
        let owner = parse_optional_string(&args, "owner");
        let metadata = args
            .get("metadata")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        let task = self
            .manager
            .create_task(
                &list_id,
                subject.clone(),
                description,
                active_form,
                owner,
                metadata,
            )
            .await?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Task #{} created: {}",
                task.id, task.subject
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "task": &task,
                "taskId": task.id,
                "listId": task.list_id,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryTaskStore, InMemoryTeamStore};
    use reflect_tools::Tool;

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    #[tokio::test]
    async fn create_returns_id_and_metadata() {
        let tool = TaskCreateTool::new(mgr());
        let ctx = ToolContext::default();
        let out = tool
            .execute(
                ctx,
                serde_json::json!({
                    "subject": "write tests",
                    "description": "add unit tests for TaskCreate"
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["taskId"], 1);
        assert_eq!(
            out.metadata["task"]["subject"],
            serde_json::json!("write tests")
        );
        assert_eq!(out.metadata["task"]["status"], "pending");
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("Task #1 created"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn create_assigns_output_path() {
        let tool = TaskCreateTool::new(mgr());
        let ctx = ToolContext::default();
        let out = tool
            .execute(ctx, serde_json::json!({"subject": "x"}))
            .await
            .unwrap();
        let path = out.metadata["task"]["output_path"]
            .as_str()
            .expect("output_path present");
        assert!(path.ends_with("/1.output.md"), "got: {path}");
    }

    #[tokio::test]
    async fn create_requires_subject() {
        let tool = TaskCreateTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn create_with_explicit_list_id() {
        let tool = TaskCreateTool::new(mgr());
        let ctx = ToolContext::default();
        let out = tool
            .execute(ctx, serde_json::json!({"subject": "x", "listId": "rocket"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["listId"], "rocket");
        assert_eq!(out.metadata["task"]["list_id"], "rocket");
    }

    #[tokio::test]
    async fn ids_increment_per_list() {
        let tool = TaskCreateTool::new(mgr());
        let ctx = ToolContext::default();
        let out1 = tool
            .execute(
                ctx.clone(),
                serde_json::json!({"subject": "a", "listId": "L1"}),
            )
            .await
            .unwrap();
        let out2 = tool
            .execute(ctx, serde_json::json!({"subject": "b", "listId": "L2"}))
            .await
            .unwrap();
        assert_eq!(out1.metadata["taskId"], 1);
        assert_eq!(out2.metadata["taskId"], 1);
    }

    #[test]
    fn metadata_is_stable() {
        // 协议级元数据必须稳定 —— M1+ queue / TUI 依赖。
        let tool = TaskCreateTool::new(mgr());
        assert_eq!(tool.name(), "TaskCreate");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "subject"));
    }

    #[tokio::test]
    async fn manager_error_propagates_as_tool_error() {
        // Phase 0 sanity:确保 `From<TaskError> for ToolError` 正常生效。
        let m = TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        );
        let err = m.get_task(&"L".into(), 999, false).await.unwrap_err();
        let tool_err: ToolError = err.into();
        assert!(matches!(tool_err, ToolError::Execution(_)));
    }
}
