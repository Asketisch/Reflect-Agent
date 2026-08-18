//! `TaskList` — 列出指定 list 下的任务(默认排除软删)。
//!
//! 可选过滤:`ownerFilter` / `statusFilter` / `includeDeleted`。
//! 返回 `text = "<N> tasks"` 一行 summary,`metadata.tasks` 完整 task 列表,
//! `metadata.count = N`。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::model::TaskStatus;
use crate::tools::{list_id_from_args_or_ctx, parse_optional_string};

/// `TaskList` 工具实现。
pub struct TaskListTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskListTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskListTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskListTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "列出当前 list(默认 session_id)的所有任务,按 id 升序。\
         可选 ownerFilter / statusFilter / includeDeleted。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "listId": {
                    "type": "string",
                    "description": "List id; defaults to current session id"
                },
                "includeDeleted": {
                    "type": "boolean",
                    "default": false,
                    "description": "Include soft-deleted tasks (status=deleted)"
                },
                "ownerFilter": {
                    "type": "string",
                    "description": "Only include tasks owned by this agent id"
                },
                "statusFilter": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "deleted"],
                    "description": "Only include tasks with this status"
                }
            }
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 纯读,可并发。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let include_deleted = args
            .get("includeDeleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let owner_filter = parse_optional_string(&args, "ownerFilter");
        let status_filter = args
            .get("statusFilter")
            .and_then(|v| v.as_str())
            .map(parse_status_filter)
            .transpose()?;

        // store 默认排除软删;`include_deleted = true` 时把 store 过滤
        // 看成"不可恢复",保留为 false 的安全值 —— 这是显式语义而非
        // 后端能力扩展点。Phase 0 store 不透传 include_deleted 参数。
        let mut tasks = self.manager.list_tasks(&list_id, include_deleted).await?;

        // 应用 owner / status 过滤
        if let Some(owner) = owner_filter {
            tasks.retain(|t| t.owner.as_deref() == Some(owner.as_str()));
        }
        if let Some(st) = status_filter {
            tasks.retain(|t| t.status == st);
        }

        let count = tasks.len();
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "{count} tasks in list '{list_id}'"
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "tasks": &tasks,
                "count": count,
                "listId": list_id,
            }),
            elapsed_ms: 0,
        })
    }
}

fn parse_status_filter(s: &str) -> Result<TaskStatus, ToolError> {
    match s {
        "pending" => Ok(TaskStatus::Pending),
        "in_progress" => Ok(TaskStatus::InProgress),
        "completed" => Ok(TaskStatus::Completed),
        "deleted" => Ok(TaskStatus::Deleted),
        other => Err(ToolError::InvalidArgs {
            message: format!("invalid statusFilter '{other}'"),
        }),
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
    async fn list_returns_all_visible() {
        let m = mgr();
        seed(&m, "L", "a").await;
        seed(&m, "L", "b").await;
        let tool = TaskListTool::new(m);
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 2);
        let arr = out.metadata["tasks"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["subject"], "a");
    }

    #[tokio::test]
    async fn list_owner_filter() {
        let m = mgr();
        let a = seed(&m, "L", "alpha").await;
        seed(&m, "L", "beta").await;
        m.update_task(
            &"L".into(),
            a,
            crate::manager::TaskPatch {
                owner: Some(Some("architect@L".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tool = TaskListTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"listId": "L", "ownerFilter": "architect@L"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 1);
        assert_eq!(out.metadata["tasks"][0]["subject"], "alpha");
    }

    #[tokio::test]
    async fn list_status_filter() {
        let m = mgr();
        let id = seed(&m, "L", "x").await;
        seed(&m, "L", "y").await;
        m.update_task(
            &"L".into(),
            id,
            crate::manager::TaskPatch {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tool = TaskListTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"listId": "L", "statusFilter": "completed"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 1);
    }

    #[tokio::test]
    async fn list_empty_returns_zero() {
        let m = mgr();
        let tool = TaskListTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"listId": "nope"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 0);
        assert!(out.metadata["tasks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskListTool::new(mgr());
        assert_eq!(tool.name(), "TaskList");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
    }
}
