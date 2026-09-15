//! OpenAI Responses API —— `/v1/responses` 流式实现(P2 `openai-responses`)。
//!
//! 与 Chat Completions(`/v1/chat/completions`)并行,通过
//! `[openai] responses_api = true` 在 builder 阶段切换。
//!
//! Responses API 的 SSE 事件类型（类型化 `event:` 字段，`data:` 携带 JSON）：
//! - `response.created` / `response.in_progress` → 映射到 `MessageStart`；
//! - `response.output_text.delta` → 映射到 `ContentDelta`；
//! - `response.output_item.added`（`function_call`）→ 映射到 `ToolUseStart`；
//! - `response.function_call_arguments.delta` → 映射到 `ToolUseDelta`；
//! - `response.completed` → 映射到 `Usage` + `MessageStop`。
//!
//! 非 OpenAI 兼容网关若不支持 Responses,缺省 `responses_api = false` 走 Chat Completions。

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::EventStream;
use futures::{Stream, StreamExt};
use reqwest::Response;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::capabilities::Capabilities;
use crate::client::ModelClient;
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::request::{ChatRequest, ContentBlock, ToolSpec};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Responses API 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponsesConfig {
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// 模型 id,如 `gpt-4o`。
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_model() -> String {
    "gpt-4o".into()
}

impl Default for OpenAIResponsesConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: None,
            timeout_secs: default_timeout_secs(),
            model: default_model(),
        }
    }
}

/// OpenAI Responses API client(`/v1/responses` 流式)。
pub struct OpenAIResponsesClient {
    config: OpenAIResponsesConfig,
    http: reqwest::Client,
}

impl OpenAIResponsesClient {
    pub fn new(config: OpenAIResponsesConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(LlmError::from)?;
        Ok(Self { config, http })
    }

    pub fn endpoint_path(&self) -> &'static str {
        "/v1/responses"
    }
}

#[async_trait]
impl ModelClient for OpenAIResponsesClient {
    fn name(&self) -> &str {
        "openai-responses"
    }

    fn provider_kind(&self) -> crate::ProviderKind {
        crate::ProviderKind::OpenAI
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            vision: true,
            json_mode: true,
            ..Capabilities::default()
        }
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let body: Value = serde_json::to_value(ResponsesRequest::from(request))
            .map_err(|e| LlmError::Internal(e.to_string()))?;

        let base = self
            .config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}{}", self.endpoint_path());

        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(body.to_string());

        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        req = req.header("X-Request-Id", uuid::Uuid::new_v4().to_string());

        let response = req.send().await?;
        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(classify_status(status_code, &text, &headers));
        }

        Ok(Box::pin(stream_sse(response, cancel)))
    }
}

fn classify_status(status: u16, body: &str, headers: &reqwest::header::HeaderMap) -> LlmError {
    match status {
        401 => LlmError::Auth,
        429 => {
            let retry_after_ms = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0) as u64)
                .unwrap_or(1000);
            LlmError::RateLimited { retry_after_ms }
        }
        529 => LlmError::Overloaded {
            retry_after_ms: 1000,
        },
        400 if body.contains("context_length_exceeded") => {
            LlmError::ContextLengthExceeded { used: 0, limit: 0 }
        }
        400 => LlmError::InvalidRequest {
            message: body.to_string(),
        },
        500..=599 => LlmError::Provider {
            status,
            message: body.to_string(),
        },
        _ => LlmError::Provider {
            status,
            message: body.to_string(),
        },
    }
}

// ── SSE → ChatEvent(类型化 `event:` 字段路由)────────────────────────────

fn stream_sse(
    response: Response,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ChatEvent, LlmError>> + Send {
    let byte_stream = response.bytes_stream();
    let mut sse = EventStream::new(byte_stream).peekable();

    async_stream::stream! {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    yield Ok(ChatEvent::Error(LlmError::Cancelled));
                    return;
                }
                next = sse.next() => {
                    let Some(item) = next else { return };
                    let evt = match item {
                        Ok(ev) => ev,
                        Err(e) => {
                            yield Err(LlmError::SseParse(e.to_string()));
                            return;
                        }
                    };
                    // Responses SSE:`[DONE]` 不出现(用 response.completed 收尾),
                    // 但兼容个别网关仍发 [DONE]。
                    if evt.data == "[DONE]" {
                        return;
                    }
                    let val: Value = match serde_json::from_str(&evt.data) {
                        Ok(v) => v,
                        Err(e) => {
                            yield Err(LlmError::SseParse(format!("invalid JSON: {e}")));
                            return;
                        }
                    };
                    for ev in parse_responses_event(&evt.event, &val) {
                        yield Ok(ev);
                    }
                    // response.completed 是终态事件,收到即结束流。
                    if evt.event == "response.completed" {
                        return;
                    }
                }
            }
        }
    }
}

/// 把单个 Responses SSE 事件(`event` 类型 + `data` JSON)映射为 0..N 个 ChatEvent。
fn parse_responses_event(event: &str, v: &Value) -> Vec<ChatEvent> {
    let mut out = Vec::new();
    match event {
        "response.created" | "response.in_progress" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");
            if !id.is_empty() || !model.is_empty() {
                out.push(ChatEvent::MessageStart {
                    id: id.to_string(),
                    model: model.to_string(),
                });
            }
        }
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(|x| x.as_str()) {
                if !delta.is_empty() {
                    out.push(ChatEvent::ContentDelta(delta.to_string()));
                }
            }
        }
        "response.output_item.added" => {
            // function_call 开始:item.function_call.{name,call_id}。
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(|x| x.as_str()) == Some("function_call") {
                    let id = item.get("call_id").and_then(|x| x.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    if !id.is_empty() || !name.is_empty() {
                        out.push(ChatEvent::ToolUseStart {
                            id: id.to_string(),
                            name: name.to_string(),
                            input_json: String::new(),
                        });
                    }
                }
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = v.get("delta").and_then(|x| x.as_str()) {
                if !delta.is_empty() {
                    out.push(ChatEvent::ToolUseDelta(delta.to_string()));
                }
            }
        }
        "response.completed" => {
            // usage 在 `response.usage`：`output_tokens` / `input_tokens` /
            // `input_tokens_details.cached_tokens`。
            if let Some(usage) = v.get("usage") {
                let input = usage
                    .get("input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                let output = usage
                    .get("output_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                let cached = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                out.push(ChatEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cached_tokens: cached,
                    cache_write_tokens: 0,
                });
            }
            out.push(ChatEvent::MessageStop);
        }
        // OpenAI Responses API reasoning 流(o-series):
        //   response.reasoning_text.delta          — 原始推理文本增量
        //   response.reasoning_summary_text.delta  — 推理摘要增量
        // 两者都映射到 ThinkingDelta(kind 由 stream.rs 固定为 "raw")。
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(|x| x.as_str()) {
                if !delta.is_empty() {
                    out.push(ChatEvent::ThinkingDelta(delta.to_string()));
                }
            }
        }
        // 其他事件(web_search 等)忽略,不影响主流。
        _ => {}
    }
    out
}

// ── ChatRequest → Responses 请求体 ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// v1.4 B1:结构化输出(`text.format` 子对象)。`None` 不序列化,
    /// 历史请求体不变。
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Value>,
}

impl From<ChatRequest> for ResponsesRequest {
    fn from(req: ChatRequest) -> Self {
        // Responses 用 `input`(message 数组)替代 `messages`;system 拆到
        // `instructions`(顶层)。
        let instructions = req.system.as_single_string();
        let mut input: Vec<Value> = Vec::new();
        for m in req.messages {
            push_input_message(m, &mut input);
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| match t {
                ToolSpec::Function {
                    name,
                    description,
                    parameters,
                } => serde_json::json!({
                    "type": "function",
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }),
            })
            .collect();
        // v1.4 B1:结构化输出 → Responses `text.format`。Responses 的
        // wire 形态与 Chat Completions 的 `response_format` 同构。
        let text = req.response_format.as_ref().map(|rf| {
            let format = match rf {
                crate::request::ResponseFormat::Text => {
                    serde_json::json!({"type": "text"})
                }
                crate::request::ResponseFormat::JsonObject => {
                    serde_json::json!({"type": "json_object"})
                }
                crate::request::ResponseFormat::JsonSchema {
                    name,
                    schema,
                    strict,
                } => serde_json::json!({
                    "type": "json_schema",
                    "name": name,
                    "schema": schema,
                    "strict": strict,
                }),
            };
            serde_json::json!({"format": format})
        });
        Self {
            model: req.model,
            input,
            tools,
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
            top_p: req.top_p,
            stream: true,
            instructions,
            text,
        }
    }
}

/// 把 ChatMessage 追加到 Responses `input` 数组(role + content)。
fn push_input_message(m: crate::request::ChatMessage, out: &mut Vec<Value>) {
    use crate::request::ChatMessage;
    match m {
        ChatMessage::System(s) => {
            // system 在 Responses 走顶层 `instructions`,但若消息流里混入
            // 额外 system(罕见),作为 role=system message 保留兼容。
            out.push(serde_json::json!({"role": "system", "content": s}));
        }
        ChatMessage::User(u) => {
            let content = blocks_to_responses_content(u.blocks);
            out.push(serde_json::json!({"role": "user", "content": content}));
        }
        ChatMessage::Assistant(a) => {
            // AssistantContent 有 text + tool_calls;Responses 助手消息
            // 用 output_text + function_call 数组。
            let mut parts: Vec<Value> = Vec::new();
            if let Some(t) = a.text {
                if !t.is_empty() {
                    parts.push(serde_json::json!({"type": "output_text", "text": t}));
                }
            }
            for tc in &a.tool_calls {
                parts.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments.to_string(),
                }));
            }
            let content = if parts.is_empty() {
                Value::String(String::new())
            } else if parts.len() == 1
                && parts[0].get("type").and_then(|x| x.as_str()) == Some("output_text")
            {
                parts[0]["text"].clone()
            } else {
                Value::Array(parts)
            };
            out.push(serde_json::json!({"role": "assistant", "content": content}));
        }
        ChatMessage::Tool(t) => {
            // tool 结果:Responses 用 role=function + call_id。`output` 为
            // 字符串,用 content_as_text()(Image 块占位,而非字节数组文本)。
            out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": t.call_id,
                "output": t.content_as_text(),
            }));
        }
    }
}

/// ContentBlock 列表 → Responses content(text 直出字符串,其余走数组)。
fn blocks_to_responses_content(blocks: Vec<ContentBlock>) -> Value {
    // 纯文本单块 → 直接字符串(最常见路径,省 token)。
    if blocks.len() == 1 {
        if let ContentBlock::Text { text } = &blocks[0] {
            return Value::String(text.clone());
        }
    }
    let arr: Vec<Value> = blocks
        .into_iter()
        .map(|b| match b {
            ContentBlock::Text { text } => {
                serde_json::json!({"type": "input_text", "text": text})
            }
            ContentBlock::Image { .. } => {
                // Responses 用 input_image;image_url 详情序列化进去。
                serde_json::json!({"type": "input_image", "image_url": "<image>"})
            }
        })
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_is_v1_responses() {
        let c = OpenAIResponsesClient::new(OpenAIResponsesConfig::default()).unwrap();
        assert_eq!(c.endpoint_path(), "/v1/responses");
    }

    #[test]
    fn parse_created_emits_message_start() {
        let v = json!({"id": "resp_123", "model": "gpt-4o"});
        let evs = parse_responses_event("response.created", &v);
        assert!(matches!(
            evs[0],
            ChatEvent::MessageStart { ref id, ref model } if id == "resp_123" && model == "gpt-4o"
        ));
    }

    #[test]
    fn parse_text_delta_emits_content_delta() {
        let v = json!({"delta": "Hello"});
        let evs = parse_responses_event("response.output_text.delta", &v);
        assert!(matches!(
            evs[0],
            ChatEvent::ContentDelta(ref s) if s == "Hello"
        ));
    }

    #[test]
    fn parse_output_item_added_function_call_emits_tool_use_start() {
        let v = json!({
            "item": {"type": "function_call", "call_id": "call_1", "name": "search"}
        });
        let evs = parse_responses_event("response.output_item.added", &v);
        assert!(matches!(
            evs[0],
            ChatEvent::ToolUseStart { ref id, ref name, .. } if id == "call_1" && name == "search"
        ));
    }

    #[test]
    fn parse_function_call_arguments_delta_emits_tool_use_delta() {
        let v = json!({"delta": "{\"q\":"});
        let evs = parse_responses_event("response.function_call_arguments.delta", &v);
        assert!(matches!(
            evs[0],
            ChatEvent::ToolUseDelta(ref s) if s == "{\"q\":"
        ));
    }

    #[test]
    fn parse_completed_emits_usage_and_stop() {
        let v = json!({
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 3}
            }
        });
        let evs = parse_responses_event("response.completed", &v);
        // 期望事件顺序：`[`Usage`, `MessageStop`]。
        assert!(matches!(
            evs[0],
            ChatEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 3,
                ..
            }
        ));
        assert!(matches!(evs[1], ChatEvent::MessageStop));
    }

    #[test]
    fn parse_unknown_event_yields_nothing() {
        let v = json!({"foo": "bar"});
        let evs = parse_responses_event("response.web_search.foo", &v);
        assert!(evs.is_empty());
    }

    #[test]
    fn parse_reasoning_text_delta_yields_thinking() {
        let v = json!({"delta": "thinking step"});
        let evs = parse_responses_event("response.reasoning_text.delta", &v);
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            ChatEvent::ThinkingDelta(ref s) if s == "thinking step"
        ));
    }

    #[test]
    fn parse_reasoning_summary_text_delta_yields_thinking() {
        let v = json!({"delta": "summary"});
        let evs = parse_responses_event("response.reasoning_summary_text.delta", &v);
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            ChatEvent::ThinkingDelta(ref s) if s == "summary"
        ));
    }

    #[test]
    fn responses_request_includes_instructions_and_input() {
        use crate::request::{ChatMessage, ChatRequest, SystemBlock, SystemBlocks, UserContent};
        let req = ChatRequest {
            model: "gpt-4o".into(),
            system: SystemBlocks(vec![SystemBlock {
                text: "be brief".into(),
                cache_control: None,
                ephemeral: false,
            }]),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::Text { text: "hi".into() }],
            })],
            ..Default::default()
        };
        let r = ResponsesRequest::from(req);
        assert_eq!(r.instructions.as_deref(), Some("be brief"));
        // input 应有 1 条 user message,content 是字符串 "hi"(单文本块优化)。
        assert_eq!(r.input.len(), 1);
        assert_eq!(r.input[0]["role"], "user");
        assert_eq!(r.input[0]["content"], "hi");
        assert!(r.stream);
    }
    // ── v1.4 B1:text.format 映射 ────────────────────────────────

    #[test]
    fn responses_json_schema_sets_text_format() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            response_format: Some(crate::ResponseFormat::JsonSchema {
                name: "answer".into(),
                schema: serde_json::json!({"type": "object"}),
                strict: false,
            }),
            ..Default::default()
        };
        let wire = serde_json::to_value(ResponsesRequest::from(req)).unwrap();
        assert_eq!(wire["text"]["format"]["type"], "json_schema");
        assert_eq!(wire["text"]["format"]["name"], "answer");
        assert_eq!(wire["text"]["format"]["strict"], false);
    }

    #[test]
    fn responses_no_response_format_no_text() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            ..Default::default()
        };
        let wire = serde_json::to_value(ResponsesRequest::from(req)).unwrap();
        assert!(wire.get("text").is_none());
    }
}
