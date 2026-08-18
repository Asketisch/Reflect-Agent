//! OpenAI provider —— Chat Completions 流式。
//!
//! 请求体:`POST /v1/chat/completions`,`stream: true`。解析 SSE 响应
//! (`data: {...}` 分块)并映射为 `ChatEvent`。

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::EventStream;
use futures::{Stream, StreamExt};
use reqwest::Response;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::capabilities::Capabilities;
use crate::client::ModelClient;
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::request::{
    ChatMessage, ChatRequest, ContentBlock, ReasoningEffort, ThinkingConfig, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

#[derive(Debug, Clone)]
pub struct OpenAIConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub timeout: Duration,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: None,
            timeout: Duration::from_secs(60),
        }
    }
}

pub struct OpenAIClient {
    config: OpenAIConfig,
    http: reqwest::Client,
}

impl OpenAIClient {
    pub fn new(config: OpenAIConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(LlmError::from)?;
        Ok(Self { config, http })
    }
}

#[async_trait]
impl ModelClient for OpenAIClient {
    fn name(&self) -> &str {
        "openai"
    }

    fn provider_kind(&self) -> crate::ProviderKind {
        crate::ProviderKind::OpenAI
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            // OpenAI 服务端自动缓存 ≥1024 token 的 prompt 前缀;响应里的
            // `cached_tokens` 经 `parse_chunk` 已读回,pricing 的
            // `cache_read_factor: 0.5` 计价正常。注入器对 OpenAI 是纯 no-op
            //(`From<ChatRequest>` 不读 `system`/`cache_control`/`metadata`),
            // 标 `true` 仅表示「本 provider 有缓存语义」,不影响出站 wire。
            prompt_caching: true,
            extended_thinking: false,
            vision: true,
            json_mode: true,
            system_blocks: false,
        }
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let body: Value = serde_json::to_value(OpenAIRequest::from(request))
            .map_err(|e| LlmError::Internal(e.to_string()))?;

        let base = self
            .config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/v1/chat/completions");

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
            let retry_after_ms = parse_retry_after_ms(headers).unwrap_or(1000);
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

fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get("retry-after")?.to_str().ok()?;
    v.parse::<f64>().ok().map(|s| (s * 1000.0) as u64)
}

// ── SSE → ChatEvent ──────────────────────────────────────────────────────────

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
                    for ev in parse_chunk(&val) {
                        yield Ok(ev);
                    }
                }
            }
        }
    }
}

fn parse_chunk(v: &Value) -> Vec<ChatEvent> {
    let mut out = Vec::new();
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");

    if !id.is_empty() || !model.is_empty() {
        out.push(ChatEvent::MessageStart {
            id: id.to_string(),
            model: model.to_string(),
        });
    }

    if let Some(usage) = v.get("usage") {
        if usage.is_object() {
            let input = usage
                .get("prompt_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            let output = usage
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            out.push(ChatEvent::Usage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                // M8: OpenAI Chat Completions 没有 `cache_creation_input_tokens`
                // (auto-prompt-cache 的命中全部走 `cached_tokens`)。
                cache_write_tokens: 0,
            });
        }
    }

    if let Some(choices) = v.get("choices").and_then(|x| x.as_array()) {
        for choice in choices {
            let finish = choice.get("finish_reason").and_then(|x| x.as_str());
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(|x| x.as_str()) {
                    if !content.is_empty() {
                        out.push(ChatEvent::ContentDelta(content.to_string()));
                    }
                }
                // reasoning_content:DeepSeek / Qwen / 部分 OpenAI o-series 兼容端点
                // 用此字段流式输出思考过程(区别于正文 content)。此前被忽略,
                // 导致这些模型的 reasoning 在 provider 层就已丢失。
                if let Some(rc) = delta.get("reasoning_content").and_then(|x| x.as_str()) {
                    if !rc.is_empty() {
                        out.push(ChatEvent::ThinkingDelta(rc.to_string()));
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if !id.is_empty() || !name.is_empty() {
                            out.push(ChatEvent::ToolUseStart {
                                id: id.to_string(),
                                name: name.to_string(),
                                input_json: String::new(),
                            });
                        }
                        if !args.is_empty() {
                            out.push(ChatEvent::ToolUseDelta(args.to_string()));
                        }
                    }
                }
            }
            if finish.is_some() && finish != Some("null") {
                // OpenAI 通过 `finish_reason == "length"` 表示输出被截断。
                // 单独上报该事件,让引擎能自动续接。
                if finish == Some("length") {
                    out.push(ChatEvent::MessageStopTruncated {
                        stop_reason: "length".to_string(),
                    });
                } else {
                    out.push(ChatEvent::MessageStop);
                }
            }
        }
    }
    out
}

// ── ChatRequest → OpenAI request body ────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct OpenAIRequest {
    model: String,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

impl From<ChatRequest> for OpenAIRequest {
    /// 对测试 / snapshot 公开。生产代码走 `OpenAIClient::stream`。
    fn from(req: ChatRequest) -> Self {
        let mut messages = Vec::new();
        if let Some(sys) = req.system.as_single_string() {
            messages.push(serde_json::json!({"role": "system", "content": sys}));
        }
        for m in req.messages {
            push_message(m, &mut messages);
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
                    "function": {"name": name, "description": description, "parameters": parameters}
                }),
            })
            .collect();
        let reasoning_effort = match &req.thinking {
            Some(ThinkingConfig::OpenAIReasoning { effort }) => {
                Some(effort_str(*effort).to_string())
            }
            _ => None,
        };
        Self {
            model: req.model,
            messages,
            tools,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            top_p: req.top_p,
            stop: req.stop,
            stream: true,
            reasoning_effort,
        }
    }
}

fn push_message(m: ChatMessage, out: &mut Vec<Value>) {
    match m {
        ChatMessage::System(s) => out.push(serde_json::json!({"role": "system", "content": s})),
        ChatMessage::User(u) => {
            // 把文本块拼接为单字符串,适配 OpenAI 更简单的 schema。
            let mut text = String::new();
            let mut parts: Vec<Value> = Vec::new();
            for b in u.blocks {
                match b {
                    ContentBlock::Text { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t);
                    }
                    ContentBlock::Image { data, mime_type } => {
                        let b64 = base64_encode(&data);
                        parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{mime_type};base64,{b64}")}
                        }));
                    }
                }
            }
            if !parts.is_empty() {
                if !text.is_empty() {
                    parts.insert(0, serde_json::json!({"type": "text", "text": text}));
                }
                out.push(serde_json::json!({"role": "user", "content": parts}));
            } else {
                out.push(serde_json::json!({"role": "user", "content": text}));
            }
        }
        ChatMessage::Assistant(a) => {
            let mut msg = serde_json::json!({"role": "assistant"});
            if let Some(t) = a.text {
                msg["content"] = Value::String(t);
            }
            if !a.tool_calls.is_empty() {
                let tcs: Vec<Value> = a
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {"name": tc.name, "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default()}
                        })
                    })
                    .collect();
                msg["tool_calls"] = Value::Array(tcs);
            }
            out.push(msg);
        }
        ChatMessage::Tool(t) => {
            // OpenAI 的 tool-role `content` 只接受字符串(不支持 tool 结果
            // 内联图片)。用 content_as_text() 发文本;Image 块显示占位
            // (好过此前把整张图拍平成字节数组文本)。
            out.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": t.call_id,
                "content": t.content_as_text(),
            }));
        }
    }
}

fn effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

// 小型 base64 编码器(避免额外依赖)。
fn base64_encode(input: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPH[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

// (无 async_stream import —— stream_sse 内通过路径使用该宏)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{SystemBlocks, UserContent};

    #[test]
    fn base64_roundtrip_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn openai_request_basic() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            })],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: Some(0.5),
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(OpenAIRequest::from(req)).unwrap();
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["stream"], true);
        assert_eq!(v["temperature"], 0.5);
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn parse_chunk_text_and_usage() {
        let chunk = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "delta": {"content": "Hello"},
                "finish_reason": null
            }]
        });
        let evs = parse_chunk(&chunk);
        assert!(
            evs.iter()
                .any(|e| matches!(e, ChatEvent::MessageStart { .. }))
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, ChatEvent::ContentDelta(s) if s == "Hello"))
        );

        let stop = serde_json::json!({
            "choices": [{"finish_reason": "stop"}]
        });
        let evs = parse_chunk(&stop);
        assert!(evs.iter().any(|e| matches!(e, ChatEvent::MessageStop)));

        let usage = serde_json::json!({
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 3}}
        });
        let evs = parse_chunk(&usage);
        assert!(matches!(
            evs.as_slice(),
            [ChatEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 3,
                cache_write_tokens: 0,
            }]
        ));
    }

    #[test]
    fn classify_status_401() {
        let h = reqwest::header::HeaderMap::new();
        let e = classify_status(401, "bad", &h);
        assert!(matches!(e, LlmError::Auth));
    }

    #[test]
    fn classify_status_429_honors_retry_after() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "5".parse().unwrap());
        let e = classify_status(429, "rate", &h);
        match e {
            LlmError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 5000),
            _ => panic!("expected RateLimited"),
        }
    }

    #[test]
    fn classify_status_context_length() {
        let h = reqwest::header::HeaderMap::new();
        let e = classify_status(400, r#"{"error":{"code":"context_length_exceeded"}}"#, &h);
        assert!(matches!(e, LlmError::ContextLengthExceeded { .. }));
    }

    /// v1.0.0-rc2：`OpenAIClient` 重写 `provider_kind() = OpenAI`。
    #[test]
    fn provider_kind_reports_openai() {
        let client = OpenAIClient::new(OpenAIConfig::default()).unwrap();
        assert_eq!(client.provider_kind(), crate::ProviderKind::OpenAI);
        assert_eq!(client.name(), "openai");
    }

    /// v1.x: OpenAI `prompt_caching` 标 `true` —— 表示本 provider 有缓存
    /// 语义(服务端自动缓存 ≥1024 token 前缀)。注入器对 OpenAI 是纯 no-op
    /// (`From<ChatRequest>` 不读 `system`/`cache_control`/`metadata`),
    /// 故出站 body 无 `cache_control` 字段(wire 安全)。
    #[test]
    fn openai_capabilities_advertise_prompt_caching() {
        let client = OpenAIClient::new(OpenAIConfig::default()).unwrap();
        assert!(
            client.capabilities().prompt_caching,
            "OpenAI 应标 prompt_caching = true"
        );
    }

    /// 出站 body 不含 `cache_control` 字段 —— `From<ChatRequest>` 不读
    /// `cache_control`/`system`/`metadata`,故翻转 `prompt_caching` 标志对
    /// wire 是纯 no-op(OpenAI 服务端自动缓存,无需显式 cache breakpoint)。
    #[test]
    fn openai_request_body_has_no_cache_control() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            })],
            tools: vec![],
            // 故意填 cache_control —— 证明 From<ChatRequest> 不读它。
            system: SystemBlocks(vec![crate::request::SystemBlock {
                text: "sys".into(),
                cache_control: Some(crate::request::CacheControl {
                    kind: crate::request::CacheControlKind::Ephemeral,
                    ttl: Some(crate::request::CacheTtl::OneHour),
                }),
                ephemeral: false,
            }]),
            temperature: None,
            max_tokens: None,
            top_p: None,
            thinking: None,
            cache_control: vec![crate::request::CacheBreak {
                after_message_index: 0,
                ttl: crate::request::CacheTtl::OneHour,
            }],
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("cache_break_tool".to_string(), "true".to_string());
                m
            },
            stop: vec![],
        };
        let v: Value = serde_json::to_value(OpenAIRequest::from(req)).unwrap();
        let body = v.to_string();
        assert!(
            !body.contains("cache_control"),
            "OpenAI 出站 body 不应含 cache_control,wire 是 no-op;got: {body}"
        );
    }
}
