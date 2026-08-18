//! `TaskGet` — 按 id 查单个任务的完整 JSON。
//!
//! 纯读查询工具,无副作用,无副作用钩子。
//! 返回 `text` 给 LLM 一行概要 + `metadata.task` 完整结构。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::tools::{list_id_from_args_or_ctx, parse_u32_arg};

/// `TaskGet` 工具实现。
pub struct TaskGetTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskGetTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskGetTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskGetTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "按 taskId 查询任务的完整 JSON 表示(含 status / blocks / blocked_by / \
         metadata / output_path 等)。可选 listId 切换列表,默认 session_id。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Task id to fetch"
                },
                "listId": {
                    "type": "string",
                    "description": "List id; defaults to current session id"
                },
                "includeDeleted": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, return soft-deleted tasks (status=deleted) as well"
                }
            },
            "required": ["taskId"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 纯读,manager 内部 RwLock 保护,并发安全。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let task_id = parse_u32_arg(&args, "taskId")?;
        let include_deleted = args
            .get("includeDeleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let task = self
            .manager
            .get_task(&list_id, task_id, include_deleted)
            .await?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Task #{}: {} ({})",
                task.id, task.subject, task.status
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
    use crate::{InMemoryTaskStore, InMemoryTeamStore, TaskStatus};
    use reflect_tools::Tool;

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    async fn seed(m: &TaskManager, list: &str, subject: &str) -> u32 {
        let t = m
            .create_task(
                &list.into(),
                subject.into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        t.id
    }

    #[tokio::test]
    async fn get_returns_full_task() {
        let m = mgr();
        let id = seed(&m, "L", "alpha").await;
        let tool = TaskGetTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["taskId"], id);
        assert_eq!(out.metadata["task"]["subject"], "alpha");
        assert_eq!(out.metadata["task"]["status"], "pending");
    }

    #[tokio::test]
    async fn get_missing_returns_error() {
        let m = mgr();
        let tool = TaskGetTool::new(m);
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": 999, "listId": "L"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn get_soft_deleted_excluded_by_default() {
        let m = mgr();
        let id = seed(&m, "L", "alpha").await;
        m.update_task(
            &"L".into(),
            id,
            crate::manager::TaskPatch {
                status: Some(TaskStatus::Deleted),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tool = TaskGetTool::new(m.clone());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn get_soft_deleted_included_when_flag_set() {
        let m = mgr();
        let id = seed(&m, "L", "alpha").await;
        m.update_task(
            &"L".into(),
            id,
            crate::manager::TaskPatch {
                status: Some(TaskStatus::Deleted),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tool = TaskGetTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L", "includeDeleted": true}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["task"]["status"], "deleted");
    }

    #[tokio::test]
    async fn requires_task_id() {
        let tool = TaskGetTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskGetTool::new(mgr());
        assert_eq!(tool.name(), "TaskGet");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
    }
}
