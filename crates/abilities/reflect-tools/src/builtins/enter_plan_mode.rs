//! `EnterPlanMode` —— v1.x Plan mode 控制面工具。
//!
//! agent 在判断任务需要规划时调用,触发 `EventMsg::PlanRequest`。
//! 注意:此工具**仅发起请求**,实际 `PermissionMode::Plan` 切换由
//! `submission_loop` 在用户审批通过后完成(Phase 4 落地)。
//!
//! 设计要点(参见 `docs/PLAN_MODE.md` 草案):
//! - `required_permission = Auto`:agent 可在任意 mode 下发起请求
//! - `is_concurrency_safe = true`:无副作用,可并发
//! - v1.x:`PlanModeGate` 在 Plan 模式下 **deny** `EnterPlanMode`(若 LLM
//!   在 Plan 模式下重复调用,会触发无意义的 `PlanRequest` 弹窗 —— 用户
//!   已通过 Shift+Tab 进入 Plan 模式,无需再次确认)。`tool_exec` 节点
//!   也做防御性兜底:检测到已在 Plan 模式时跳过 PlanRequest 派发。

use async_trait::async_trait;
use reflect_protocol::PermissionMode;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};

pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.EnterPlanMode")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "规划目标的简短描述(如 'refactor auth module')"
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    /// v1.x:Plan mode 控制面工具自身使用 `Auto`,因为它不修改任何文件,
    /// 只是请求一次 mode 切换。Plan mode gate (`PlanModeGate` builtin
    /// hook)会把它列入白名单,允许在 Plan 模式下调(虽然语义上无意义)。
    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: Value,
    ) -> Result<reflect_protocol::ToolOutput, ToolError> {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "EnterPlanMode requires a non-empty 'task' string".into(),
            })?
            .to_string();
        if task.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "EnterPlanMode 'task' must not be empty".into(),
            });
        }
        // 注意:此处不直接 emit `EventMsg::PlanRequest`,由 submission_loop
        // 在 ToolCallEnd 阶段按 tool_name == "EnterPlanMode" 派发
        // (MCP `McpToolInvoked` 同 pattern)。
        Ok(reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Plan mode requested for task: {task}"
            ))],
            is_error: false,
            metadata: serde_json::json!({"requested_task": task}),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::ToolOutput;

    #[tokio::test]
    async fn enter_plan_mode_returns_request_confirmation() {
        let t = EnterPlanModeTool;
        let ctx = ToolContext::default();
        let out = t
            .execute(ctx, serde_json::json!({"task": "refactor auth module"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("refactor auth module"));
            }
            _ => panic!("expected text block"),
        }
        assert_eq!(out.metadata["requested_task"], "refactor auth module");
    }

    #[test]
    fn enter_plan_mode_metadata_is_stable() {
        // 协议级元数据(name / permission / concurrency)供 hook 引擎和
        // ToolExecutionQueue 决策,必须稳定。
        let t = EnterPlanModeTool;
        assert_eq!(t.name(), "EnterPlanMode");
        assert!(t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Auto);
        assert!(t.description().contains("Plan mode"));
        let schema = t.parameters_schema();
        assert_eq!(schema["required"][0], "task");
    }

    #[tokio::test]
    async fn enter_plan_mode_rejects_missing_task() {
        let t = EnterPlanModeTool;
        let ctx = ToolContext::default();
        let err = t.execute(ctx, serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        if let ToolError::InvalidArgs { message } = err {
            assert!(message.contains("task"));
        }
    }

    #[tokio::test]
    async fn enter_plan_mode_rejects_empty_task() {
        let t = EnterPlanModeTool;
        let ctx = ToolContext::default();
        let err = t
            .execute(ctx, serde_json::json!({"task": "  "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn enter_plan_mode_rejects_non_string_task() {
        let t = EnterPlanModeTool;
        let ctx = ToolContext::default();
        let err = t
            .execute(ctx, serde_json::json!({"task": 123}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    /// 验证 `ToolOutput` 公共字段(确保编译期契约不变)。
    #[test]
    fn enter_plan_mode_tool_output_shape() {
        let out = ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text("x")],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        };
        assert_eq!(out.elapsed_ms, 0);
    }
}
