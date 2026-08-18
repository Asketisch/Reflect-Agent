//! `TaskUpdate` — 局部更新任务字段。
//!
//! 任意字段 patch;`status=completed` 时
//! 触发 `TaskCompleted` 钩子,其他变更触发 `TaskUpdated` 钩子(由 manager 派发)。
//!
//! `required_permission = Prompt` —— 任务状态变更属于持久化副作用,需要
//! 人工审批;`status=completed` 二次确认由 `action_permission` 单独判断。
//!
//! ## Status 解码
//!
//! `status` 是字符串而非枚举,因为 schema 上更宽松(`"pending" | "in_progress"
//! | "completed" | "deleted"`)由 tool 自己解析并映射到 `TaskStatus` 枚举。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::{TaskManager, TaskPatch, UpdateOutcome};
use crate::model::TaskStatus;
use crate::tools::{list_id_from_args_or_ctx, parse_optional_string, parse_u32_arg};

/// `TaskUpdate` 工具实现。
pub struct TaskUpdateTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskUpdateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskUpdateTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskUpdateTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

/// 把字符串 status 解码成 `TaskStatus`,无效字符串返回 `InvalidArgs`。
fn parse_status(s: &str) -> Result<TaskStatus, ToolError> {
    match s {
        "pending" => Ok(TaskStatus::Pending),
        "in_progress" => Ok(TaskStatus::InProgress),
        "completed" => Ok(TaskStatus::Completed),
        "deleted" => Ok(TaskStatus::Deleted),
        other => Err(ToolError::InvalidArgs {
            message: format!(
                "invalid status '{other}'; expected one of pending|in_progress|completed|deleted"
            ),
        }),
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "局部更新任务字段(subject / description / activeForm / status / owner \
         / metadata / blocks / blockedBy)。返回 updatedFields 与 statusChange。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "integer", "minimum": 1, "description": "Task id"},
                "listId": {"type": "string", "description": "List id; defaults to current session id"},
                "subject": {"type": "string", "description": "Replace subject"},
                "description": {"type": "string", "description": "Replace description"},
                "activeForm": {"type": ["string", "null"], "description": "Replace active form (null to clear)"},
                "owner": {"type": ["string", "null"], "description": "Replace owner (null to clear)"},
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "deleted"],
                    "description": "Transition task status"
                },
                "metadata": {"type": "object", "additionalProperties": true, "description": "Replace metadata"},
                "addBlocks": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Append task ids that this task blocks"
                },
                "addBlockedBy": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Append task ids that block this task"
                }
            },
            "required": ["taskId"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 写操作,跨调用顺序敏感(尤其 addBlocks / addBlockedBy)。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // 持久化状态变更需 prompt 审批。
        PermissionMode::Prompt
    }

    /// v1.0.0-rc1+:状态切换独立路由 —— `status=completed` 才走 Prompt,
    /// 普通字段微调(改改 subject)默认 Auto,降低 LLM 摩擦。
    fn action_permission(&self, args: &Value) -> PermissionMode {
        match args.get("status").and_then(|v| v.as_str()) {
            Some("completed") | Some("deleted") => PermissionMode::Prompt,
            _ => self.required_permission(),
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let task_id = parse_u32_arg(&args, "taskId")?;

        // ── patch 装配 ──
        let mut patch = TaskPatch::default();
        if let Some(s) = parse_optional_string(&args, "subject") {
            patch.subject = Some(s);
        }
        if let Some(s) = parse_optional_string(&args, "description") {
            patch.description = Some(s);
        }
        // activeForm / owner 支持 null 显式清空,直接读原始 Value。
        if let Some(v) = args.get("activeForm") {
            patch.active_form = Some(v.as_str().map(|s| Some(s.to_string())).unwrap_or(None));
        }
        if let Some(v) = args.get("owner") {
            patch.owner = Some(v.as_str().map(|s| Some(s.to_string())).unwrap_or(None));
        }
        if let Some(s) = args.get("status").and_then(|v| v.as_str()) {
            let st = parse_status(s)?;
            patch.status = Some(st);
        }
        if let Some(m) = args.get("metadata") {
            patch.metadata = Some(m.clone());
        }
        if let Some(arr) = args.get("addBlocks").and_then(|v| v.as_array()) {
            let mut ids = Vec::with_capacity(arr.len());
            for v in arr {
                let id = v.as_u64().ok_or_else(|| ToolError::InvalidArgs {
                    message: "addBlocks entries must be integers".into(),
                })? as u32;
                ids.push(id);
            }
            patch.add_blocks = Some(ids);
        }
        if let Some(arr) = args.get("addBlockedBy").and_then(|v| v.as_array()) {
            let mut ids = Vec::with_capacity(arr.len());
            for v in arr {
                let id = v.as_u64().ok_or_else(|| ToolError::InvalidArgs {
                    message: "addBlockedBy entries must be integers".into(),
                })? as u32;
                ids.push(id);
            }
            patch.add_blocked_by = Some(ids);
        }

        let outcome = self.manager.update_task(&list_id, task_id, patch).await?;

        Ok(task_update_output(outcome))
    }
}

/// 把 `UpdateOutcome` 渲染成 `ToolOutput` —— text 给一行人类可读 summary,
/// metadata 包含 task 快照 + updatedFields + statusChange(便于 LLM 与
/// TUI reducer 各自消费)。
pub(crate) fn task_update_output(outcome: UpdateOutcome) -> ToolOutput {
    let mut summary = format!("Updated task #{}", outcome.task.id);
    if !outcome.updated_fields.is_empty() {
        summary.push_str(&format!(": {}", outcome.updated_fields.join(", ")));
    }
    if let Some((from, to)) = outcome.status_change {
        summary.push_str(&format!(" (status: {from} → {to})"));
    }

    ToolOutput {
        content: vec![reflect_protocol::ContentBlock::text(summary)],
        is_error: false,
        metadata: serde_json::json!({
            "task": &outcome.task,
            "taskId": outcome.task.id,
            "listId": outcome.task.list_id,
            "updatedFields": outcome.updated_fields,
            "statusChange": outcome.status_change.map(|(f, t)| {
                (TaskStatus::to_string(&f).to_string(), TaskStatus::to_string(&t).to_string())
            }),
        }),
        elapsed_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::TaskPatch;
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
    async fn update_subject_persists() {
        let m = mgr();
        let id = seed(&m, "L", "old").await;
        let tool = TaskUpdateTool::new(m.clone());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L", "subject": "new"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["updatedFields"][0], "subject");
        let t = m.get_task(&"L".into(), id, false).await.unwrap();
        assert_eq!(t.subject, "new");
    }

    #[tokio::test]
    async fn status_completed_emits_status_change() {
        let m = mgr();
        let id = seed(&m, "L", "x").await;
        let tool = TaskUpdateTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": id, "listId": "L", "status": "completed"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["statusChange"][0], "pending");
        assert_eq!(out.metadata["statusChange"][1], "completed");
    }

    #[tokio::test]
    async fn invalid_status_rejected() {
        let tool = TaskUpdateTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": 1, "listId": "L", "status": "bogus"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn active_form_null_clears_field() {
        let m = mgr();
        let id = seed(&m, "L", "x").await;
        m.update_task(
            &"L".into(),
            id,
            TaskPatch {
                active_form: Some(Some("doing".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let tool = TaskUpdateTool::new(m.clone());
        tool.execute(
            ToolContext::default(),
            serde_json::json!({"taskId": id, "listId": "L", "activeForm": null}),
        )
        .await
        .unwrap();
        let t = m.get_task(&"L".into(), id, false).await.unwrap();
        assert!(t.active_form.is_none());
    }

    #[tokio::test]
    async fn add_blocks_appends() {
        let m = mgr();
        let a = seed(&m, "L", "a").await;
        let b = seed(&m, "L", "b").await;
        let tool = TaskUpdateTool::new(m.clone());
        tool.execute(
            ToolContext::default(),
            serde_json::json!({"taskId": a, "listId": "L", "addBlocks": [b]}),
        )
        .await
        .unwrap();
        let t = m.get_task(&"L".into(), a, false).await.unwrap();
        assert_eq!(t.blocks, vec![b]);
    }

    #[tokio::test]
    async fn action_permission_prompt_for_status() {
        let tool = TaskUpdateTool::new(mgr());
        let perm = tool.action_permission(&serde_json::json!({
            "taskId": 1,
            "status": "completed"
        }));
        assert_eq!(perm, PermissionMode::Prompt);
        let perm2 = tool.action_permission(&serde_json::json!({
            "taskId": 1,
            "subject": "x"
        }));
        assert_eq!(perm2, PermissionMode::Prompt); // required_permission
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskUpdateTool::new(mgr());
        assert_eq!(tool.name(), "TaskUpdate");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
    }
}
