//! `ChatEvent` —— 模型返回的流式事件。
//!
//! 与具体 provider 解耦。各 provider 把自身 SSE 协议翻译成这些事件;
//! `reflect-core` 再把它们翻译为 `EventMsg` 事件。

use serde::{Deserialize, Serialize};

use crate::error::LlmError;

/// v1.4 B2:一次调用的 token 用量快照(`ChatEvent::Usage` 的具名形态),
/// 供 `ModelClient::complete` 的非流式产出携带。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_tokens: u32,
    pub cache_write_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// 首个分块,携带响应 id 与模型名。
    MessageStart { id: String, model: String },

    /// 助手返回的增量文本内容。
    ContentDelta(String),

    /// 工具调用块的起始事件。
    ToolUseStart {
        id: String,
        name: String,
        input_json: String,
    },

    /// 当前工具调用块的增量 JSON 参数。
    ToolUseDelta(String),

    /// 增量思考内容(Anthropic extended thinking)。
    ThinkingDelta(String),

    /// 流正常结束(模型停止本轮,或一个 tool_use 块已完成)。
    MessageStop,

    /// 流因输出触达 provider 的 `max_tokens` 上限而终止
    /// (Anthropic `stop_reason == "max_tokens"`,OpenAI
    /// `finish_reason == "length"`)。携带原始 stop_reason 字符串,
    /// 让引擎能区分**截断**回合(输出未完整)与**完成**回合。
    /// 单独设为变体(而非 `MessageStop` 上的字段),是为了让现有约 40 处
    /// `MessageStop` 匹配 / 构造点保持源码兼容。
    MessageStopTruncated { stop_reason: String },

    /// Token 用量快照。`cached_tokens` 是 cache_read 折扣段
    /// (属于 `input_tokens` 的子集);`cache_write_tokens` 是
    /// cache_creation 段(同样计入 `input_tokens`)。
    /// 不支持 prompt 缓存的 provider 上两者均为 0。
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cached_tokens: u32,
        cache_write_tokens: u32,
    },

    /// 流中途的错误(未必终止整条流)。
    Error(LlmError),
}

impl ChatEvent {
    /// 稳定的字符串判别符。
    pub fn discriminant(&self) -> &'static str {
        match self {
            ChatEvent::MessageStart { .. } => "message_start",
            ChatEvent::ContentDelta(_) => "content_delta",
            ChatEvent::ToolUseStart { .. } => "tool_use_start",
            ChatEvent::ToolUseDelta(_) => "tool_use_delta",
            ChatEvent::ThinkingDelta(_) => "thinking_delta",
            ChatEvent::MessageStop => "message_stop",
            ChatEvent::MessageStopTruncated { .. } => "message_stop_truncated",
            ChatEvent::Usage { .. } => "usage",
            ChatEvent::Error(_) => "error",
        }
    }
}
