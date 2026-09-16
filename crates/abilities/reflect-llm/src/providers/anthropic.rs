//! Anthropic provider —— Messages 流式 + prompt 缓存。
//!
//! 请求体:`POST /v1/messages`,`stream: true`。解析 SSE 事件流
//! (类型化事件如 `message_start`、`content_block_start`、
//! `content_block_delta`、`message_delta`、`message_stop`)。

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
    CacheControl, CacheControlKind, CacheTtl, ChatMessage, ChatRequest, ContentBlock,
    ThinkingConfig, ToolSpec,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// M8:尾部需要附加 `cache_control: ephemeral` 的工具数。
/// Anthropic 单次请求最多支持 4 个 cache breakpoint;我们保留最后一个给
/// system prompt,剩余 3 个用于工具定义。
const CACHE_TAIL_TOOLS: usize = 3;

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub timeout: Duration,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: None,
            timeout: Duration::from_secs(60),
        }
    }
}

pub struct AnthropicClient {
    config: AnthropicConfig,
    http: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(LlmError::from)?;
        Ok(Self { config, http })
    }
}

#[async_trait]
impl ModelClient for AnthropicClient {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn provider_kind(&self) -> crate::ProviderKind {
        crate::ProviderKind::Anthropic
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            prompt_caching: true,
            extended_thinking: true,
            vision: true,
            json_mode: true, // via tool-forced JSON
            system_blocks: true,
        }
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        // v1.4 B1:结构化输出经强制工具调用实现(`AnthropicRequest::from`
        // 注入 `structured_output` 工具 + tool_choice)。此处记录是否
        // 启用,启用则对流做解包适配 —— 模型的 tool_use 参数在流侧被
        // 还原为 ContentDelta 文本,上层无感知(声明 json_mode 的既有
        // 注释 "via tool-forced JSON" 由此真正落地)。
        let structured = request.response_format.is_some();
        let body: Value = serde_json::to_value(AnthropicRequest::from(request))
            .map_err(|e| LlmError::Internal(e.to_string()))?;

        let base = self
            .config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/v1/messages");

        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }

        // 当使用自定义 base_url 时（非官方 Anthropic API），自动切换为
        // `Authorization: Bearer` 认证方式以兼容第三方网关（vLLM 等）。
        let is_official = self
            .config
            .base_url
            .as_deref()
            .map(|u| u.contains("api.anthropic.com"))
            .unwrap_or(true);
        let mut req = self.http.post(&url);
        if is_official {
            req = req.header("x-api-key", &self.config.api_key);
        } else {
            req = req.header("Authorization", format!("Bearer {}", self.config.api_key));
        }
        let response = req
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(body.to_string())
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(classify_status(status_code, &text, &headers));
        }

        if structured {
            Ok(Box::pin(unwrap_structured_output(stream_sse(
                response, cancel,
            ))))
        } else {
            Ok(Box::pin(stream_sse(response, cancel)))
        }
    }
}

/// v1.4 B1:结构化输出内部工具名(`AnthropicRequest::from` 注入,
/// 本函数的流适配层据此拦截解包)。
pub(crate) const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";

/// v1.4 B1:流侧解包 —— 把强制 `structured_output` 工具调用的参数
/// 还原为普通文本输出。
///
/// - `ToolUseStart { name: "structured_output" }`:进入捕获模式(吞掉,
///   上层不应看到这个内部工具);
/// - `ToolUseDelta`:累积参数 JSON 分片;
/// - `MessageStop` / `MessageStopTruncated`:若有捕获内容,先产出一条
///   `ContentDelta`(完整 JSON 文本)再透传终止事件 —— 上层与纯文本
///   输出的处理路径完全一致;
/// - 其余事件(MessageStart / Usage / ThinkingDelta)原样透传。
///
/// 模型未按约束调用工具时(异常路径)无捕获内容,终止事件照常透传,
/// 上层看到空文本 —— 语义同「模型没回答」,不 panic。
fn unwrap_structured_output<S>(inner: S) -> impl Stream<Item = Result<ChatEvent, LlmError>> + Send
where
    S: Stream<Item = Result<ChatEvent, LlmError>> + Send,
{
    async_stream::stream! {
        let mut inner = Box::pin(inner);
        let mut capturing = false;
        let mut json = String::new();
        while let Some(item) = inner.next().await {
            let ev = match item {
                Ok(e) => e,
                Err(e) => {
                    yield Err(e);
                    continue;
                }
            };
            match ev {
                ChatEvent::ToolUseStart { name, .. } if name == STRUCTURED_OUTPUT_TOOL => {
                    capturing = true;
                    json.clear();
                }
                ChatEvent::ToolUseDelta(d) if capturing => {
                    json.push_str(&d);
                }
                ChatEvent::MessageStop => {
                    if capturing && !json.is_empty() {
                        yield Ok(ChatEvent::ContentDelta(std::mem::take(&mut json)));
                    }
                    capturing = false;
                    yield Ok(ChatEvent::MessageStop);
                }
                ChatEvent::MessageStopTruncated { stop_reason } => {
                    if capturing && !json.is_empty() {
                        yield Ok(ChatEvent::ContentDelta(std::mem::take(&mut json)));
                    }
                    capturing = false;
                    yield Ok(ChatEvent::MessageStopTruncated { stop_reason });
                }
                other => yield Ok(other),
            }
        }
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
        400 if body.contains("prompt is too long") => {
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

// ── SSE → ChatEvent 解析 ──────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct StreamState {
    /// 当前 content block 是否为 thinking 块。
    in_thinking: bool,
    /// 累计 input tokens(取自 `message_start.message.usage.input_tokens`)。
    /// 注意:Anthropic 的 `input_tokens` 已包含
    /// `cache_creation_input_tokens` 子段。
    input_tokens: u32,
    /// 累计 output tokens(由 `message_delta.usage.output_tokens` 更新)。
    output_tokens: u32,
    /// 已缓存 input tokens(取自 `message_start.message.usage.cache_read_input_tokens`)。
    /// `input_tokens` 的子集(cache_read 折扣段)。
    cached_tokens: u32,
    /// M8:cache_creation input tokens(取自
    /// `message_start.message.usage.cache_creation_input_tokens`)。
    /// `input_tokens` 的子集(cache_write 计费段)。
    cache_write_tokens: u32,
    /// Usage 事件是否已发出(我们在 message_stop 时统一发一次)。
    usage_emitted: bool,
    /// `message_delta.delta.stop_reason`(如 `end_turn`、`max_tokens`、
    /// `tool_use`、`stop_sequence`)。在 `message_stop` 事件触发时,
    /// 若输出触达 `max_tokens`,据此上报 `MessageStopTruncated`;
    /// 引擎据此自动续接被截断的回合。
    stop_reason: Option<String>,
}

fn stream_sse(
    response: Response,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ChatEvent, LlmError>> + Send {
    let byte_stream = response.bytes_stream();
    let mut sse = EventStream::new(byte_stream).peekable();
    let mut state = StreamState::default();

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
                    // event 字段位于 evt.event;数据位于 evt.data
                    for ev in parse_sse_event(&evt.event, &evt.data, &mut state) {
                        yield Ok(ev);
                    }
                }
            }
        }
    }
}

fn parse_sse_event(event: &str, data: &str, state: &mut StreamState) -> Vec<ChatEvent> {
    let mut out = Vec::new();
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_e) => {
            // SSE 数据格式错误 —— 丢弃该事件并继续流。
            return vec![];
        }
    };
    match event {
        "message_start" => {
            let id = v
                .pointer("/message/id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let model = v
                .pointer("/message/model")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(input) = v
                .pointer("/message/usage/input_tokens")
                .and_then(|x| x.as_u64())
            {
                state.input_tokens = input as u32;
            }
            if let Some(cached) = v
                .pointer("/message/usage/cache_read_input_tokens")
                .and_then(|x| x.as_u64())
            {
                state.cached_tokens = cached as u32;
            }
            // M8:采集 cache_creation 段(按 write 档倍率计费,与 cache_read 折扣段不同)。
            if let Some(write) = v
                .pointer("/message/usage/cache_creation_input_tokens")
                .and_then(|x| x.as_u64())
            {
                state.cache_write_tokens = write as u32;
            }
            if let Some(output) = v
                .pointer("/message/usage/output_tokens")
                .and_then(|x| x.as_u64())
            {
                state.output_tokens = output as u32;
            }
            out.push(ChatEvent::MessageStart { id, model });
        }
        "content_block_start" => {
            let block_type = v
                .pointer("/content_block/type")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            state.in_thinking = block_type == "thinking";
            if block_type == "tool_use" {
                let id = v
                    .pointer("/content_block/id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = v
                    .pointer("/content_block/name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(ChatEvent::ToolUseStart {
                    id,
                    name,
                    input_json: String::new(),
                });
            }
        }
        "content_block_delta" => {
            let delta_type = v
                .pointer("/delta/type")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            match delta_type {
                "text_delta" => {
                    if let Some(text) = v.pointer("/delta/text").and_then(|x| x.as_str()) {
                        out.push(ChatEvent::ContentDelta(text.to_string()));
                    }
                }
                "input_json_delta" => {
                    if let Some(partial) = v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                    {
                        out.push(ChatEvent::ToolUseDelta(partial.to_string()));
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = v.pointer("/delta/thinking").and_then(|x| x.as_str()) {
                        out.push(ChatEvent::ThinkingDelta(text.to_string()));
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            state.in_thinking = false;
        }
        "message_delta" => {
            if let Some(output) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                state.output_tokens = output as u32;
            }
            // 采集 stop_reason;后续的 `message_stop` 事件据此区分是否被 max_tokens 截断。
            if let Some(reason) = v
                .pointer("/delta/stop_reason")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                state.stop_reason = Some(reason.to_string());
            }
            // 在 message_delta 阶段发出最终的 Usage 事件(message_stop 紧随其后)。
            if !state.usage_emitted {
                out.push(ChatEvent::Usage {
                    input_tokens: state.input_tokens,
                    output_tokens: state.output_tokens,
                    cached_tokens: state.cached_tokens,
                    cache_write_tokens: state.cache_write_tokens,
                });
                state.usage_emitted = true;
            }
        }
        "message_stop" => {
            // `max_tokens` ⇒ 输出在生成中途被截断;以独立事件上报,
            // 让引擎自动续接被截断的回合。
            if state.stop_reason.as_deref() == Some("max_tokens") {
                out.push(ChatEvent::MessageStopTruncated {
                    stop_reason: "max_tokens".to_string(),
                });
            } else {
                out.push(ChatEvent::MessageStop);
            }
        }
        _ => {
            // ping / error / 其它类型化事件 —— 忽略
        }
    }
    out
}

// ── ChatRequest → Anthropic 请求体 转换 ────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<Value>,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    /// v1.4 B1:结构化输出的强制工具选择(`{"type":"tool","name":
    /// "structured_output"}`)。`None` 不序列化,历史请求体不变。
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

impl From<ChatRequest> for AnthropicRequest {
    /// 对测试 / snapshot 公开。生产代码走 `AnthropicClient::stream`。
    fn from(req: ChatRequest) -> Self {
        // 在最后一块 system block 上注入 cache_control(若尚无)。
        let mut system_blocks: Vec<Value> = req
            .system
            .0
            .iter()
            .map(|b| {
                let mut v = serde_json::json!({"type": "text", "text": b.text});
                if let Some(cc) = b.cache_control {
                    v["cache_control"] = cache_control_json(cc);
                }
                v
            })
            .collect();
        if !system_blocks.is_empty()
            && !system_blocks
                .last()
                .and_then(|v| v.get("cache_control"))
                .is_some()
        {
            let cc = serde_json::json!({"type": "ephemeral", "ttl": "5m"});
            system_blocks.last_mut().unwrap()["cache_control"] = cc;
        }

        // 构造 messages
        let mut messages = Vec::new();
        for m in req.messages {
            push_message(m, &mut messages);
        }

        // M8:对 prompt 中靠后的 N 条 message content blocks 附加
        // `cache_control: ephemeral`(由上游 `reflect_prompt::inject_cache_control`
        // 写入 `req.cache_control: Vec<CacheBreak>`)。跳过 thinking 块
        // —— Anthropic API 拒绝在 thinking 内容上设置 cache_control。
        let message_break_indices: std::collections::HashSet<usize> = req
            .cache_control
            .iter()
            .map(|b| b.after_message_index)
            .filter(|&i| i < messages.len())
            .collect();
        if !message_break_indices.is_empty() {
            for idx in &message_break_indices {
                if let Some(msg) = messages.get_mut(*idx) {
                    attach_cache_break_to_message(msg, req.thinking.is_some());
                }
            }
        }

        // 工具定义(Anthropic 没有 `type:"function"` 外层)
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| match t {
                ToolSpec::Function {
                    name,
                    description,
                    parameters,
                } => serde_json::json!({
                    "name": name,
                    "description": description,
                    "input_schema": parameters,
                }),
            })
            .collect();

        // M8:尾部 N 个工具附加 `cache_control: ephemeral`(由 `caching.rs`
        // 通过 `req.metadata["cache_break_tool"]` 标记启用)。
        // 这是在 system prompt 之后收益最高的 cache breakpoint —— 工具定义
        // 体积大,且回合间几乎不变。
        if !tools.is_empty()
            && req.metadata.get("cache_break_tool").map(String::as_str) == Some("true")
        {
            let start = tools.len().saturating_sub(CACHE_TAIL_TOOLS);
            for t in &mut tools[start..] {
                t["cache_control"] = serde_json::json!({"type": "ephemeral", "ttl": "5m"});
            }
        }

        // v1.4 B1:结构化输出 —— Anthropic 无原生 response_format,经强制
        // 工具调用实现:注入 `structured_output` 工具(schema 来自请求的
        // JsonSchema;JsonObject 用「任意对象」宽松 schema)+ `tool_choice`
        // 强制选择。流侧由 `unwrap_structured_output` 把参数解包回文本,
        // 上层无感知。
        let tool_choice = match &req.response_format {
            Some(crate::request::ResponseFormat::JsonSchema { name, schema, .. }) => {
                tools.push(serde_json::json!({
                    "name": STRUCTURED_OUTPUT_TOOL,
                    "description": format!("Output the final answer as a JSON value conforming to this schema ({name})."),
                    "input_schema": schema,
                }));
                Some(serde_json::json!({"type": "tool", "name": STRUCTURED_OUTPUT_TOOL}))
            }
            Some(crate::request::ResponseFormat::JsonObject) => {
                tools.push(serde_json::json!({
                    "name": STRUCTURED_OUTPUT_TOOL,
                    "description": "Output the final answer as a single JSON object.",
                    "input_schema": serde_json::json!({"type": "object"}),
                }));
                Some(serde_json::json!({"type": "tool", "name": STRUCTURED_OUTPUT_TOOL}))
            }
            // `Text` 与 `None`:纯文本,不注入内部工具。
            _ => None,
        };

        // Anthropic 要求提供 max_tokens
        let max_tokens = req.max_tokens.unwrap_or(4096);

        // Thinking —— 启用时 temperature 必须为 1。
        let thinking = match &req.thinking {
            Some(ThinkingConfig::Enabled { budget_tokens }) => Some(serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget_tokens
            })),
            Some(ThinkingConfig::OpenAIReasoning { .. }) => None,
            _ => None,
        };
        let temperature = if thinking.is_some() {
            // Anthropic 要求启用 thinking 时 temperature=1。
            Some(1.0)
        } else {
            req.temperature
        };

        Self {
            model: req.model,
            system: system_blocks,
            messages,
            tools,
            max_tokens,
            temperature,
            top_p: req.top_p,
            stop_sequences: req.stop,
            stream: true,
            thinking,
            tool_choice,
        }
    }
}

/// M8:为 Anthropic 消息的最后一块 content block 附加 `cache_control: ephemeral`。
/// `thinking_enabled = true` 时跳过 thinking 块 —— Anthropic API 拒绝在
/// thinking 内容上设置 cache_control。
/// - `{role: "user", content: [tool_result]}`:附加到 tool_result 块;
/// - `{role: "user", content: [text,...]}`:附加到最后一块 text;
/// - `{role: "assistant", content: [text|tool_use|thinking]}`:附加到最后一块
///   非 thinking 块(全为 thinking 时跳过)。
fn attach_cache_break_to_message(msg: &mut Value, thinking_enabled: bool) {
    let cc = || serde_json::json!({"type": "ephemeral", "ttl": "5m"});
    if let Some(content_arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
        // 从右向左遍历 content blocks;取首个非 thinking 块(在启用 thinking 时)。
        for block in content_arr.iter_mut().rev() {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if thinking_enabled && block_type == "thinking" {
                continue;
            }
            // 仅在 Anthropic API 接受 cache_control 的类型上附加:
            // text / image / tool_use / tool_result / document。
            // 跳过 thinking(已过滤)和任何未知类型。
            if matches!(
                block_type,
                "text" | "image" | "tool_use" | "tool_result" | "document"
            ) {
                block["cache_control"] = cc();
                return;
            }
            // 未知类型 —— 放弃,以避免发送无效的 wire 形状。
            return;
        }
    }
}

fn cache_control_json(cc: CacheControl) -> Value {
    let mut v = serde_json::json!({"type": match cc.kind {
        CacheControlKind::Ephemeral => "ephemeral",
    }});
    if let Some(ttl) = cc.ttl {
        v["ttl"] = match ttl {
            CacheTtl::FiveMinutes => Value::String("5m".into()),
            CacheTtl::OneHour => Value::String("1h".into()),
        };
    } else {
        v["ttl"] = Value::String("5m".into());
    }
    v
}

fn push_message(m: ChatMessage, out: &mut Vec<Value>) {
    match m {
        ChatMessage::System(s) => {
            // System 在 Anthropic 中是顶层字段,不是消息。
            // 若意外出现 ChatMessage::System,直接忽略
            // (我们已在顶层处理过 `request.system`)。
            let _ = s;
        }
        ChatMessage::User(u) => {
            let content: Vec<Value> = u
                .blocks
                .into_iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    ContentBlock::Image { data, mime_type } => {
                        let b64 = base64_encode(&data);
                        serde_json::json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mime_type, "data": b64}
                        })
                    }
                })
                .collect();
            out.push(serde_json::json!({"role": "user", "content": content}));
        }
        ChatMessage::Assistant(a) => {
            let mut content: Vec<Value> = Vec::new();
            if let Some(t) = a.text {
                content.push(serde_json::json!({"type": "text", "text": t}));
            }
            if let Some(think) = a.thinking {
                content.push(serde_json::json!({"type": "thinking", "thinking": think}));
            }
            for tc in a.tool_calls {
                content.push(serde_json::json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": tc.arguments,
                }));
            }
            out.push(serde_json::json!({"role": "assistant", "content": content}));
        }
        ChatMessage::Tool(t) => {
            // Anthropic 的 `tool_result.content` 原生支持 content-blocks
            // 数组(可含 text 与 image 块)。把 typed blocks 映射成 Anthropic
            // 格式 —— Image 块走 base64 source,使模型能真正"看到"
            // `image_view` 工具返回的图片(此前 content 被拍平成字节数组
            // 字符串,模型看到的不是图而是 `[137,80,78,71,...]` 文本)。
            let blocks: Vec<Value> = t
                .content
                .into_iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    ContentBlock::Image { data, mime_type } => {
                        let b64 = base64_encode(&data);
                        serde_json::json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mime_type, "data": b64}
                        })
                    }
                })
                .collect();
            out.push(serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": t.call_id,
                    "content": blocks,
                    "is_error": t.is_error,
                }]
            }));
        }
    }
}

// 小型 base64 编码器(与 OpenAI 模块相同;为 crate 内部清晰度复制一份)。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CacheBreak, ContentBlock as CB, SystemBlock, SystemBlocks, UserContent};

    #[test]
    fn system_gets_cache_control_injected() {
        let req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![],
            tools: vec![],
            system: SystemBlocks(vec![SystemBlock {
                text: "you are helpful".into(),
                cache_control: None,
                ephemeral: false,
            }]),
            temperature: None,
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let system = v["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(
            system[0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "5m"})
        );
    }

    #[test]
    fn existing_cache_control_is_preserved() {
        let req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![],
            tools: vec![],
            system: SystemBlocks(vec![SystemBlock {
                text: "x".into(),
                cache_control: Some(CacheControl {
                    kind: CacheControlKind::Ephemeral,
                    ttl: Some(CacheTtl::OneHour),
                }),
                ephemeral: false,
            }]),
            temperature: None,
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert_eq!(
            v["system"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn thinking_forces_temperature_one() {
        let req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: Some(0.5),
            max_tokens: Some(1024),
            top_p: None,
            thinking: Some(ThinkingConfig::Enabled {
                budget_tokens: 1024,
            }),
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert_eq!(v["temperature"], 1.0);
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn max_tokens_defaults_to_4096() {
        let req = ChatRequest {
            response_format: None,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert_eq!(v["max_tokens"], 4096);
    }

    #[test]
    fn parse_message_start_emits_message_start_event() {
        let mut state = StreamState::default();
        let data = r#"{"message":{"id":"msg_1","model":"claude-3-5-sonnet-latest","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":7}}}"#;
        let evs = parse_sse_event("message_start", data, &mut state);
        assert!(
            matches!(&evs[0], ChatEvent::MessageStart { id, model } if id == "msg_1" && model.starts_with("claude"))
        );
        assert_eq!(state.input_tokens, 10);
        assert_eq!(state.cached_tokens, 7);
    }

    #[test]
    fn parse_content_block_delta_text_and_tool() {
        let mut state = StreamState::default();
        // text delta(测试用 SSE delta 类型字符串)
        let data = r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let evs = parse_sse_event("content_block_delta", data, &mut state);
        assert!(matches!(&evs[0], ChatEvent::ContentDelta(s) if s == "Hello"));

        // tool_use start(测试用 SSE event 类型字符串)
        let data = r#"{"index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"},"delta":{}}"#;
        let evs = parse_sse_event("content_block_start", data, &mut state);
        assert!(
            matches!(&evs[0], ChatEvent::ToolUseStart { id, name, .. } if id == "toolu_1" && name == "bash")
        );

        // input_json_delta(测试用 SSE delta 类型字符串)
        let data = r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#;
        let evs = parse_sse_event("content_block_delta", data, &mut state);
        assert!(matches!(&evs[0], ChatEvent::ToolUseDelta(s) if s.contains("cmd")));
    }

    #[test]
    fn parse_thinking_delta() {
        let mut state = StreamState::default();
        let data = r#"{"index":0,"content_block":{"type":"thinking","thinking":""},"delta":{}}"#;
        parse_sse_event("content_block_start", data, &mut state);
        assert!(state.in_thinking);
        let data = r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        let evs = parse_sse_event("content_block_delta", data, &mut state);
        assert!(matches!(&evs[0], ChatEvent::ThinkingDelta(s) if s == "hmm"));
    }

    #[test]
    fn classify_status_401_and_429() {
        let h = reqwest::header::HeaderMap::new();
        assert!(matches!(classify_status(401, "", &h), LlmError::Auth));
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "3".parse().unwrap());
        match classify_status(429, "", &h) {
            LlmError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 3000),
            _ => panic!(),
        }
    }

    #[test]
    fn message_stop_emits_stop() {
        let mut state = StreamState::default();
        let evs = parse_sse_event("message_stop", "{}", &mut state);
        assert!(matches!(&evs[0], ChatEvent::MessageStop));
    }

    // ── M8：cache_control 接线（wire-up）───────────────────────────────────

    fn make_req_with_tools_and_cache() -> ChatRequest {
        let mut req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![CB::text("hi")],
            })],
            tools: (0..5)
                .map(|i| ToolSpec::Function {
                    name: format!("t{i}"),
                    description: "".into(),
                    parameters: serde_json::json!({"type": "object"}),
                })
                .collect(),
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        req.metadata
            .insert("cache_break_tool".to_string(), "true".to_string());
        req
    }

    #[test]
    fn from_request_attaches_cache_control_to_last_n_tools() {
        let req = make_req_with_tools_and_cache();
        let v = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        // 前 (5 - 3 = 2) 个工具未变;最后 3 个获得 cache_control。
        for (i, t) in tools.iter().enumerate() {
            let has_cc = t.get("cache_control").is_some();
            if i < 2 {
                assert!(!has_cc, "tool[{i}] should NOT have cache_control, got {t}");
            } else {
                assert!(has_cc, "tool[{i}] should have cache_control, got {t}");
                assert_eq!(t["cache_control"]["type"], "ephemeral");
                assert_eq!(t["cache_control"]["ttl"], "5m");
            }
        }
    }

    #[test]
    fn from_request_skips_tool_cache_control_when_metadata_flag_missing() {
        let mut req = make_req_with_tools_and_cache();
        req.metadata.remove("cache_break_tool");
        let v = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let tools = v["tools"].as_array().unwrap();
        for (i, t) in tools.iter().enumerate() {
            assert!(
                t.get("cache_control").is_none(),
                "tool[{i}] should NOT have cache_control when flag absent, got {t}"
            );
        }
    }

    #[test]
    fn from_request_anchor_message_gets_cache_control() {
        use crate::request::CacheBreak;
        let req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![
                ChatMessage::User(UserContent {
                    blocks: vec![CB::text("first")],
                }),
                ChatMessage::Assistant(crate::request::AssistantContent {
                    text: Some("second".into()),
                    tool_calls: vec![],
                    thinking: None,
                }),
                ChatMessage::User(UserContent {
                    blocks: vec![CB::text("third — anchor")],
                }),
            ],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![CacheBreak {
                after_message_index: 2,
                ttl: CacheTtl::FiveMinutes,
            }],
            metadata: Default::default(),
            stop: vec![],
        };
        let v = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let messages = v["messages"].as_array().unwrap();
        // 第三条消息的最后一块 content block 带 cache_control。
        let last = messages[2]["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last["cache_control"]["type"], "ephemeral");
        assert_eq!(last["cache_control"]["ttl"], "5m");
        // 前两条消息未变。
        for (i, m) in messages.iter().enumerate().take(2) {
            let blocks = m["content"].as_array().unwrap();
            for b in blocks {
                assert!(
                    b.get("cache_control").is_none(),
                    "messages[{i}] should not have cache_control, got {b}"
                );
            }
        }
    }

    #[test]
    fn from_request_anchor_skips_thinking_block_when_thinking_enabled() {
        // M8:启用 thinking 时,锚点 `attach_cache_break_to_message` 辅助函数
        // 必须跳过 thinking 块(Anthropic API 拒绝在 thinking 内容上设置
        // cache_control)。手工构造一条带 [text, thinking] 的 assistant 消息,
        // 断言只有 text 块获得 cache_control。
        use crate::request::AssistantContent;
        let req = ChatRequest {
            response_format: None,
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![ChatMessage::Assistant(AssistantContent {
                text: Some("hi".into()),
                tool_calls: vec![],
                thinking: Some("internal monologue".into()),
            })],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: Some(1024),
            top_p: None,
            thinking: Some(ThinkingConfig::Enabled {
                budget_tokens: 1024,
            }),
            cache_control: vec![CacheBreak {
                after_message_index: 0,
                ttl: CacheTtl::FiveMinutes,
            }],
            metadata: Default::default(),
            stop: vec![],
        };
        let v = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let blocks = v["messages"][0]["content"].as_array().unwrap();
        // 顺序:[text, thinking] —— text 应有 cache_control,thinking 不应有。
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["type"], "thinking");
        assert!(
            blocks[1].get("cache_control").is_none(),
            "thinking block must NOT carry cache_control, got {blocks:?}"
        );
    }

    /// v1.0.0-rc2：`AnthropicClient` 重写 `provider_kind() = Anthropic`。
    #[test]
    fn provider_kind_reports_anthropic() {
        let client = AnthropicClient::new(AnthropicConfig::default()).unwrap();
        assert_eq!(client.provider_kind(), crate::ProviderKind::Anthropic);
        assert_eq!(client.name(), "anthropic");
    }
    // ── v1.4 B1:结构化输出的强制工具注入与流解包 ────────────────

    #[test]
    fn anthropic_structured_output_injects_tool_and_tool_choice() {
        let req = ChatRequest {
            model: "claude-3-5-sonnet-latest".into(),
            response_format: Some(crate::ResponseFormat::JsonSchema {
                name: "verdict".into(),
                schema: serde_json::json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}),
                strict: true,
            }),
            ..Default::default()
        };
        let wire = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        let tools = wire["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "只注入 structured_output 一个工具");
        assert_eq!(tools[0]["name"], "structured_output");
        assert!(
            tools[0]["input_schema"]["properties"]["ok"].is_object(),
            "schema 原样进入 input_schema"
        );
        assert_eq!(wire["tool_choice"]["type"], "tool");
        assert_eq!(wire["tool_choice"]["name"], "structured_output");
    }

    #[test]
    fn anthropic_no_response_format_no_tool_choice() {
        let req = ChatRequest {
            model: "claude-3-5-sonnet-latest".into(),
            ..Default::default()
        };
        let wire = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
        assert!(wire.get("tool_choice").is_none());
        assert!(
            wire["tools"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true)
        );
    }

    /// 流解包:structured_output 的 tool_use 参数被还原为 ContentDelta,
    /// 内部工具名对上层不可见。
    #[tokio::test]
    async fn unwrap_structured_output_restores_text() {
        use futures::StreamExt;
        let events = vec![
            Ok(ChatEvent::MessageStart {
                id: "m".into(),
                model: "claude".into(),
            }),
            Ok(ChatEvent::ToolUseStart {
                id: "t1".into(),
                name: STRUCTURED_OUTPUT_TOOL.into(),
                input_json: String::new(),
            }),
            Ok(ChatEvent::ToolUseDelta(r#""ok": true"#.into())),
            Ok(ChatEvent::MessageStop),
        ];
        let inner = futures::stream::iter(events);
        let out: Vec<ChatEvent> = unwrap_structured_output(inner)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        // 期望:MessageStart → ContentDelta(完整 JSON 文本) → MessageStop。
        // 内部 ToolUseStart / ToolUseDelta 不应透传。
        let mut saw_json = false;
        for ev in &out {
            match ev {
                ChatEvent::ContentDelta(t) if t.contains("\"ok\"") => saw_json = true,
                ChatEvent::ToolUseStart { .. } | ChatEvent::ToolUseDelta(_) => {
                    panic!("内部工具事件不应透传: {ev:?}")
                }
                _ => {}
            }
        }
        assert!(saw_json, "应产出包含 JSON 的 ContentDelta,实际: {out:?}");
        assert!(matches!(out.last(), Some(ChatEvent::MessageStop)));
    }
}
