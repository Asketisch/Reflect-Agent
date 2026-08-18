//! `TaskStop` — 中止一个 in-progress 任务。
//!
//! 把 status 从 `in_progress` 改为
//! `pending`,清除 `owner`,让其他 agent 接手。**不**做物理删除 ——
//! 那是 `TaskUpdate status=deleted` 的语义。
//!
//! ## 权限
//!
//! `required_permission = Prompt` —— 中止他人正在执行的任务是高敏感操作,
//! 必须经过用户审批。`is_concurrency_safe = false` —— 同一任务被多 agent
//! 同时 stop 时,顺序影响 owner 字段,需要串行。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::{TaskManager, TaskPatch};
use crate::model::TaskStatus;
use crate::tools::{list_id_from_args_or_ctx, parse_u32_arg};

/// `TaskStop` 工具实现。
pub struct TaskStopTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskStopTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStopTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskStopTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "中止正在进行的任务(in_progress → pending 并清除 owner)。任务本身保留,\
         输出文件不动 —— 想要彻底删除用 TaskUpdate 把 status 设为 deleted。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "integer", "minimum": 1, "description": "Task id to stop"},
                "listId": {"type": "string", "description": "List id; defaults to current session id"}
            },
            "required": ["taskId"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // 中止他人正在执行的任务是高敏感操作。
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let task_id = parse_u32_arg(&args, "taskId")?;

        // 先确认任务存在 + 当前状态;in_progress 之外的状态由调用方
        // 通过 TaskUpdate 走更明确的路径(避免误用)。
        let current = self.manager.get_task(&list_id, task_id, false).await?;
        if current.status != TaskStatus::InProgress {
            return Err(ToolError::Execution(format!(
                "TaskStop requires status=in_progress; current status is {}",
                current.status
            )));
        }

        let patch = TaskPatch {
            status: Some(TaskStatus::Pending),
            // `owner: Some(None)` 在 TaskPatch 语义里表示"显式清空"。
            owner: Some(None),
            ..Default::default()
        };
        let outcome = self.manager.update_task(&list_id, task_id, patch).await?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Task #{} stopped: status in_progress → pending, owner cleared",
                outcome.task.id
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "task": &outcome.task,
                "taskId": outcome.task.id,
                "listId": outcome.task.list_id,
                "stoppedOwner": current.owner,
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

    async fn claim(m: &TaskManager, list: &str) -> u32 {
        let t = m
            .create_task(
                &list.into(),
                "x".into(),
                "".into(),
                None,
                Some("worker@L".into()),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        m.update_task(
            &"L".into(),
            t.id,
            crate::manager::TaskPatch {
                status: Some(TaskStatus::InProgress),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        t.id
    }

    #[tokio::test]
    async fn stop_in_progress_clears_owner_and_pending() {
        let m = mgr();
        let id = claim(&m, "L").await;
        let tool = TaskStopTool::new(m.clone());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L"}),
            )
            .await
            .unwrap();
        let t = m.get_task(&"L".into(), id, false).await.unwrap();
        assert_eq!(t.status, TaskStatus::Pending);
        assert!(t.owner.is_none());
        assert_eq!(out.metadata["stoppedOwner"], "worker@L");
    }

    #[tokio::test]
    async fn stop_pending_errors() {
        let m = mgr();
        let id = m
            .create_task(
                &"L".into(),
                "x".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap()
            .id;
        let tool = TaskStopTool::new(m);
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
    async fn stop_missing_errors() {
        let tool = TaskStopTool::new(mgr());
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
    async fn requires_task_id() {
        let tool = TaskStopTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskStopTool::new(mgr());
        assert_eq!(tool.name(), "TaskStop");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
    }
}
