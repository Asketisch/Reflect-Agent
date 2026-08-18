//! 协议层错误类型。通过 `reflect-llm` 中的 `From` 实现桥接到 `LlmError`
//! (Rust 孤儿规则要求 `impl` 必须写在本地侧)。

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Serialize, Deserialize)]
pub enum ProtocolError {
    #[error("invalid op: {reason}")]
    InvalidOp { reason: String },

    #[error("unsupported provider: {name}")]
    UnsupportedProvider { name: String },

    #[error("unsupported model: {provider}/{model}")]
    UnsupportedModel { provider: String, model: String },

    #[error("tool timeout: {tool_name} after {elapsed_ms}ms")]
    ToolTimeout { tool_name: String, elapsed_ms: u64 },

    #[error("tool not found: {tool_name}")]
    ToolNotFound { tool_name: String },

    #[error("hook rejected: {hook_name} ({reason})")]
    HookRejected { hook_name: String, reason: String },

    #[error("llm error: {0}")]
    LlmError(String),

    #[error("interrupted")]
    Interrupted,

    #[error("shutdown in progress")]
    ShutdownInProgress,

    #[error("internal: {message}")]
    Internal { message: String },

    #[error("serialization: {0}")]
    Serialization(String),

    #[error("io: {0}")]
    Io(String),
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::Serialization(e.to_string())
    }
}

impl From<std::io::Error> for ProtocolError {
    fn from(e: std::io::Error) -> Self {
        ProtocolError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_stable() {
        let e = ProtocolError::InvalidOp {
            reason: "bad".into(),
        };
        assert_eq!(e.to_string(), "invalid op: bad");

        let e = ProtocolError::UnsupportedModel {
            provider: "openai".into(),
            model: "gpt-9".into(),
        };
        assert_eq!(e.to_string(), "unsupported model: openai/gpt-9");
    }
}
