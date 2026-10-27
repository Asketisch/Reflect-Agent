//! 压缩 + 错误载荷。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompactedEvent {
    pub strategy: ContextCompactedStrategy,
    pub removed_messages: usize,
    pub before_tokens: u32,
    pub after_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StreamErrorEvent {
    pub code: String,
    pub message: String,
    pub retry_in_ms: u64,
    /// v1.0 多 Provider 路由:失败 credential 的 provider 名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// v1.0 多 Provider 路由:失败 credential 的 label。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_label: Option<String>,
    /// v1.0 多 Provider 路由:全部 candidate 试完仍失败时,记录每个
    /// 候选的结果。便于 TUI 渲染"5 个 key 都试过:work=429,
    /// personal=auth, ..."诊断表。`None` 表示未填(单 credential 失败
    /// 或仍在重试中)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tried: Option<Vec<TriedCredential>>,
}

/// v1.0 多 Provider 路由:单个 credential 失败时记录在 `StreamError.tried`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriedCredential {
    pub label: String,
    /// 失败原因码(同 `LlmError` 变体名):
    /// `"rate_limited"` / `"auth"` / `"overloaded"` /
    /// `"provider_5xx"` / `"network"` / `"context_too_long"` / ...
    pub outcome: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactedStrategy {
    /// M1 stub 使用的 no-op。
    #[default]
    Noop,
    /// 本地启发式压缩(M4)。
    Microcompact,
    /// 阈值触发的智能剪枝(M4)。
    SmartPrune,
    /// LLM 摘要压缩(M4)。
    LlMSummarize,
}
