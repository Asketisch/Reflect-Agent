//! `TaskClaim` —— 协调器 worker 自动认领任务的工具。
//!
//! 参数:`listId?`(默认 ctx.session_id)、`claimerId?`(默认
//! `self_agent_id`,即调用此工具的 worker id,格式 `<role>@<team>`)。
//!
//! 行为:调 `TaskManager::claim_next_available` 原子拉取 list 下第一个
//! `Pending + !blocked_by` 任务,把它标 `InProgress` 并填 `claimed_by` /
//! `claimed_at` / `owner`。返回 `metadata.task` 完整对象,便于 worker 后续
//! `TaskGet` / `TaskUpdate` 继续操作。
//!
//! 无可认领任务时:返回 `content: ["No claimable task available"]`,
//! `is_error = false`(LLM 看到文本即可,不视作错误)。
//!
//! 权限:`Auto`,因为本工具是 worker 自动派工的协议入口,不能弹审批。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::tools::list_id_from_args_or_ctx;

/// `TaskClaim` 工具实现。
pub struct TaskClaimTool {
    manager: Arc<TaskManager>,
    /// 调用此工具的 worker 自身的 agent id(`<role>@<team>`),用于
    /// `claimerId?` 默认值,以及 `claim_next_available` 的认领身份。
    self_agent_id: String,
}

impl std::fmt::Debug for TaskClaimTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskClaimTool")
            .field("manager", &"Arc<TaskManager>")
            .field("self_agent_id", &self.self_agent_id)
            .finish()
    }
}

impl TaskClaimTool {
    pub fn new(manager: Arc<TaskManager>, self_agent_id: impl Into<String>) -> Self {
        Self {
            manager,
            self_agent_id: self_agent_id.into(),
        }
    }
}

#[async_trait]
impl Tool for TaskClaimTool {
    fn name(&self) -> &str {
        "TaskClaim"
    }

    fn description(&self) -> &str {
        "Worker 自动认领任务。在指定 list 下原子拉取第一个 Pending 且无上游阻塞 \
         的任务,把它标 InProgress 并填 claimed_by / claimed_at / owner。\
         无可认领时返回提示文本(非错误)。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "listId": {
                    "type": "string",
                    "description": "List id; defaults to current session id"
                },
                "claimerId": {
                    "type": "string",
                    "description": "Agent id claiming the task; defaults to the calling worker id"
                }
            }
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // TaskManager::claim_next_available 内部 per-list 锁串行化,
        // 跨 list 并发安全;同 list 并发安全(锁内 find+mutate+save)。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let claimer_id = args
            .get("claimerId")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.self_agent_id)
            .to_string();

        match self
            .manager
            .claim_next_available(&list_id, &claimer_id)
            .await?
        {
            Some(task) => Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(format!(
                    "Claimed task #{}: {}",
                    task.id, task.subject
                ))],
                is_error: false,
                metadata: serde_json::json!({
                    "task": &task,
                    "taskId": task.id,
                    "listId": task.list_id,
                    "claimerId": claimer_id,
                }),
                elapsed_ms: 0,
            }),
            None => Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(
                    "No claimable task available",
                )],
                is_error: false,
                metadata: serde_json::json!({
                    "listId": list_id,
                    "claimerId": claimer_id,
                }),
                elapsed_ms: 0,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryTaskStore, InMemoryTeamStore};

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    #[tokio::test]
    async fn claim_returns_pending_task() {
        let manager = mgr();
        manager
            .create_task(
                &"L".into(),
                "alpha".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let tool = TaskClaimTool::new(manager, "architect@rocket");
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["taskId"], 1);
        assert_eq!(out.metadata["claimerId"], "architect@rocket");
        assert_eq!(out.metadata["task"]["claimed_by"], "architect@rocket");
        assert_eq!(out.metadata["task"]["status"], "in_progress");
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("Claimed task #1"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn claim_returns_no_task_message_when_empty() {
        let manager = mgr();
        let tool = TaskClaimTool::new(manager, "w@x");
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"listId": "empty"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["taskId"], Value::Null);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("No claimable task"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn claim_skips_blocked_task() {
        let manager = mgr();
        manager
            .create_task(
                &"L".into(),
                "upstream".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        manager
            .create_task(
                &"L".into(),
                "downstream".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        manager
            .update_task(
                &"L".into(),
                2,
                crate::manager::TaskPatch {
                    add_blocked_by: Some(vec![1]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let tool = TaskClaimTool::new(manager, "w@t");
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap();
        // 跳过 downstream,拿到 upstream
        assert_eq!(out.metadata["taskId"], 1);
        assert_eq!(out.metadata["task"]["subject"], "upstream");
    }

    #[tokio::test]
    async fn claim_uses_explicit_claimer_id() {
        let manager = mgr();
        manager
            .create_task(
                &"L".into(),
                "t".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let tool = TaskClaimTool::new(manager, "default@x");
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"listId": "L", "claimerId": "explicit@y"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["claimerId"], "explicit@y");
        assert_eq!(out.metadata["task"]["claimed_by"], "explicit@y");
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskClaimTool::new(mgr(), "w@x");
        assert_eq!(tool.name(), "TaskClaim");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
        let schema = tool.parameters_schema();
        // 列出 listId / claimerId 两个可选字段
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("listId"));
        assert!(props.contains_key("claimerId"));
    }
}
