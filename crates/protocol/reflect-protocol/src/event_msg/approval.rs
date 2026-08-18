//! 审批 + 权限冒泡(payload)载荷。

use serde::{Deserialize, Serialize};

use crate::item::{PlanId, RiskLevel};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequestEvent {
    /// 唯一 id;客户端会在 `Op::ToolApproval` / `Op::HookApproval` 中回传。
    pub request_id: String,
    /// 待审批的对象(工具调用或 hook 决策)。
    pub kind: ApprovalKind,
    /// 风险等级(仅供 UI 提示,不强制)。
    #[serde(default)]
    pub risk: RiskLevel,
}

/// 被审批的对象。工具调用携带工具名 + 参数;hook 审批携带 hook 名 + 决策载荷的
/// 简短可读预览。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalKind {
    Tool {
        tool_name: String,
        args: serde_json::Value,
    },
    Hook {
        hook_name: String,
        decision_preview: String,
    },
    /// v1.x Plan mode: 用户审批 plan markdown。`summary` 是 plan 的
    /// 简短预览(首 100 字符),TUI modal 用作预览文字;
    /// `plan_id` 与 `PlanReady` / `PlanApproved` / `PlanRejected` 的 id
    /// 对应,便于前端把 approval 与具体 plan 配对渲染。
    Plan { plan_id: PlanId, summary: String },
}

/// Bubble 权限模式下的非阻塞工具执行通知。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionBubbleEvent {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
    #[serde(default)]
    pub risk: RiskLevel,
}
