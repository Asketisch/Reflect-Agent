//! `TaskRelease` —— worker 主动放回任务到 `Pending`。
//!
//! 与 `TaskClaim` 对偶:清空 `claimed_by` / `claimed_at`,把 status 从
//! `InProgress` 退回 `Pending`,`owner` 保留(让 LLM 看到 owner 仍是自己,
//! 不至于出现"我自己释放了任务但 owner 没了"的诡异状态)。
//!
//! 参数:`listId?`(默认 ctx.session_id)、`taskId`(必填,被释放任务的 id)。
//!
//! 用途:
//! - worker 发现自己不适合做这个任务 → 释放让别的 worker claim。
//! - worker 启动时发现自己 claim 的任务与自身 role 不匹配 → 释放。
//! - 测试/调度场景下强制重置任务状态。
//!
//! 权限:`Auto`,与 `TaskClaim` 同 —— 都是协调器派工协议的内部工具。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::{TaskManager, TaskPatch};
use crate::model::TaskStatus;
use crate::tools::{list_id_from_args_or_ctx, parse_u32_arg};

/// `TaskRelease` 工具实现。
pub struct TaskReleaseTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskReleaseTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskReleaseTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskReleaseTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TaskReleaseTool {
    fn name(&self) -> &str {
        "TaskRelease"
    }

    fn description(&self) -> &str {
        "Worker 主动放回已认领的任务。\
         清空 claimed_by / claimed_at 并把 status 从 InProgress 退回 Pending,\
         owner 保留。释放后该任务可被其他 worker 通过 TaskClaim 重新认领。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Id of the task to release"
                },
                "listId": {
                    "type": "string",
                    "description": "List id; defaults to current session id"
                }
            },
            "required": ["taskId"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // Release 不并发安全:两次 release 同一 task 会触发"重复更新"语义,
        // 但 manager 内部 update_task 已经按 list-level 锁串行化;这里标
        // false 避免上层并发调度把 release 与 claim 重叠。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let task_id = parse_u32_arg(&args, "taskId")?;

        // 三态 patch:清空 claimed_by 与 claimed_at,把 status 退回 Pending。
        let patch = TaskPatch {
            status: Some(TaskStatus::Pending),
            claimed_by: Some(None),
            claimed_at: Some(None),
            ..Default::default()
        };
        let outcome = self.manager.update_task(&list_id, task_id, patch).await?;
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Released task #{}: {}",
                task_id, outcome.task.subject
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "task": &outcome.task,
                "taskId": task_id,
                "listId": list_id,
                "updatedFields": outcome.updated_fields,
            }),
            elapsed_ms: 0,
        })
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
    async fn release_returns_task_to_pending() {
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
        // 先 claim
        let _ = manager
            .claim_next_available(&"L".into(), "w@t")
            .await
            .unwrap();
        // 确认 claim 后状态
        let t = manager.get_task(&"L".into(), 1, false).await.unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        assert!(t.claimed_by.is_some());

        let tool = TaskReleaseTool::new(manager);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": 1, "listId": "L"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["taskId"], 1);
        assert_eq!(out.metadata["task"]["status"], "pending");
        assert_eq!(out.metadata["task"]["claimed_by"], Value::Null);
        assert!(
            out.metadata["updatedFields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "status")
        );
    }

    #[tokio::test]
    async fn release_releases_then_can_be_reclaimed() {
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

        let claim_tool = crate::tools::task_claim::TaskClaimTool::new(manager.clone(), "w1@t");
        let _ = claim_tool
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap();
        let release_tool = TaskReleaseTool::new(manager.clone());
        release_tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": 1, "listId": "L"}),
            )
            .await
            .unwrap();

        // w2@t 再 claim 应该拿到同一任务
        let claim_tool2 = crate::tools::task_claim::TaskClaimTool::new(manager, "w2@t");
        let out = claim_tool2
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["claimerId"], "w2@t");
        assert_eq!(out.metadata["taskId"], 1);
    }

    #[tokio::test]
    async fn release_requires_task_id() {
        let tool = TaskReleaseTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"listId": "L"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TaskReleaseTool::new(mgr());
        assert_eq!(tool.name(), "TaskRelease");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "taskId"));
    }

    /// v1.1.0 review P2-1:`taskId` 旧 schema 用 `"type": "u32"` —— 这是
    /// **非法的 JSON Schema** 关键字(规范只接受 `integer` / `string` /
    /// `number` / `boolean` / `object` / `array` / `null`)。严格校验
    /// schema 的 LLM 客户端(OpenAI strict mode / Anthropic tool_use
    /// 严格模式)看到 `"u32"` 会整体拒收 tool spec,导致 LLM 无法调
    /// TaskRelease —— 静默失能。修复后 `"type": "integer"` + `minimum: 1`
    /// 与同模块 `task_update` / `task_get` / `task_stop` 对齐。
    #[test]
    fn schema_uses_valid_json_schema_types() {
        let tool = TaskReleaseTool::new(mgr());
        let schema = tool.parameters_schema();
        let task_id = &schema["properties"]["taskId"];
        assert_eq!(
            task_id["type"], "integer",
            "taskId.type 必须是 'integer' (JSON Schema 关键字),不是 'u32'"
        );
        assert_eq!(
            task_id["minimum"], 1,
            "taskId 应该带 minimum: 1 防止 id=0 / 负数"
        );
    }
}
