//! LLM 输出载荷:助手消息、thinking delta、token 用量。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageDelta {
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingDelta {
    pub delta: String,
    /// P4:thinking delta 的语义类型,默认 `"raw"`(token 级流式);`"summary"`
    /// 时 TUI reducer 路由到 `MessageRow::ReasoningSummary`(区别于原始
    /// `Thinking`)。`#[serde(default)]` 保持与旧事件的线缆兼容。
    #[serde(default)]
    pub kind: String,
}

impl Default for ThinkingDelta {
    fn default() -> Self {
        Self {
            delta: String::new(),
            kind: "raw".to_string(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenCountEvent {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_tokens: u32,
    /// M8:cache_creation 段(已折入 `input_tokens`);不支持 prompt caching
    /// 的 provider 上为 0。`#[serde(default)]` 保证事件与 M7 consumer 的
    /// 线缆兼容。
    #[serde(default)]
    pub cache_write_tokens: u32,
    pub total_tokens: u32,
    /// M8 P1a:本轮 USD 费用,由 `reflect_llm::providers::pricing` 算出。
    /// 模型未知或定价表为空时为 `None`。`#[serde(default)]` 保证事件
    /// 与 M7 consumer 的线缆兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// v1.0 多 Provider 路由:本轮实际命中的 provider 名(如 `"anthropic"`)。
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` 双向
    /// 兼容:旧 consumer 解析时填 `None`,序列化时缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// v1.0 多 Provider 路由:命中的 credential label(如 `"work"` /
    /// `"default"`)。同 `provider` 字段的兼容策略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_label: Option<String>,
}
