//! v1.0 多 Provider 路由 / cooldown 事件。

use serde::{Deserialize, Serialize};

/// v1.0 多 Provider 路由:failover / cooldown 状态变化事件。TUI
/// 收到后画一行 status(`↻ main switched work → personal (rate_limited)`)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingEvent {
    pub kind: RoutingEventKind,
    /// `"main"` / `"compact"` / `"subagent:researcher"`。
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_credential: Option<String>,
    /// 同 `TriedCredential.outcome`,描述切换原因。
    pub reason: String,
    /// 距 cooldown 到期的毫秒数(可选)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingEventKind {
    /// 切到了下一个可用 credential。
    Switched,
    /// 全部 candidate 失败,无路可走。
    FailedOver,
    /// 单 credential 进 cooldown 暂避。
    CooldownStarted,
    /// 成功调用后清除 cooldown(标记恢复)。
    CooldownCleared,
}
