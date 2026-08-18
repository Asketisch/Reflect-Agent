//! LLM 错误类型 —— 各 provider 可返回的错误形式。可重试与快速失败的分类
//! 在 `reflect-core::submission_loop` 中处理。

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmError {
    /// HTTP 层传输错误。
    #[error("http: {0}")]
    Http(String),

    /// SSE 协议层解析错误。
    #[error("sse parse: {0}")]
    SseParse(String),

    /// 401 / API key 无效。
    #[error("auth failed")]
    Auth,

    /// 429 响应,携带 `Retry-After`(毫秒)。
    #[error("rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    /// 400 响应且 body 匹配 `context_length_exceeded` / `prompt is too long`。
    #[error("context length exceeded: used {used} > {limit}")]
    ContextLengthExceeded { used: u32, limit: u32 },

    /// 400 错误(非上下文超长)。
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    /// 529 响应(Anthropic overloaded)。
    #[error("provider overloaded, retry after {retry_after_ms}ms")]
    Overloaded { retry_after_ms: u64 },

    /// 5xx 响应,携带 body。
    #[error("provider error: {status} {message}")]
    Provider { status: u16, message: String },

    /// CancellationToken 触发。
    #[error("cancelled")]
    Cancelled,

    /// 兜底变体。
    #[error("internal: {0}")]
    Internal(String),
}

impl From<reqwest::Error> for LlmError {
    fn from(e: reqwest::Error) -> Self {
        LlmError::Http(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_useful() {
        assert_eq!(LlmError::Auth.to_string(), "auth failed");
        assert_eq!(
            LlmError::RateLimited {
                retry_after_ms: 1500
            }
            .to_string(),
            "rate limited, retry after 1500ms"
        );
        assert_eq!(
            LlmError::ContextLengthExceeded {
                used: 200_000,
                limit: 100_000
            }
            .to_string(),
            "context length exceeded: used 200000 > 100000"
        );
    }
}
