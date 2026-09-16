//! Ollama provider —— 原生 `/api/chat` NDJSON 流。
//!
//! 选用原生端点而非 OpenAI-compat `/v1/chat/completions`,理由:
//! - 拿到 `keep_alive` / `num_ctx` / `num_gpu` 等 Ollama 专属字段;
//! - 真 `prompt_eval_count` / `eval_count` 直接从流末尾 `done:true`
//!   chunk 拿到,无需借助 OpenAI 透传的 `prompt_tokens` / `completion_tokens`;
//! - 原生 tool-call 格式(`arguments` 是 JSON object,不是字符串);
//!
//! 流形态:每行一个 JSON 对象,不是 SSE `data:` 前缀。`stream_ndjson`
//! 用 `BufRead::lines()` 风格按 `\n` 切分,逐行 `serde_json::from_str`。

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::Response;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::capabilities::Capabilities;
use crate::client::ModelClient;
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::request::{ChatMessage, ChatRequest, ContentBlock, ToolSpec, UserContent};

/// Ollama 默认 endpoint,本地 `ollama serve` 监听端口。
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";
/// 默认模型 —— 体积小、社区维护、`ollama pull llama3.2` 一键拉。
const DEFAULT_MODEL: &str = "llama3.2";
/// 默认超时 120s;本地首次模型加载可能 30s+,比 OpenAI/Anthropic 的 60s 宽。
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Ollama server URL,默认 `http://127.0.0.1:11434`。
    pub base_url: Option<String>,
    /// 可选;Ollama Cloud / 反向代理带认证时填。
    pub api_key: Option<String>,
    /// `Some(0)` 立即卸载、`Some(-1)` 永久保留、`None` 走 server 默认 5 分钟。
    pub keep_alive_secs: Option<i64>,
    /// 上下文窗口大小(传给 Ollama `options.num_ctx`)。
    pub num_ctx: Option<u32>,
    /// GPU 层数(传给 Ollama `options.num_gpu`)。
    pub num_gpu: Option<u32>,
    /// 单次 HTTP 请求超时;本地首次模型加载可能较慢,默认 120s。
    pub timeout: Duration,
    /// 默认 model 覆盖。builder 一般已带;这里是兜底。
    pub model: Option<String>,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key: None,
            keep_alive_secs: None,
            num_ctx: None,
            num_gpu: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            model: None,
        }
    }
}

pub struct OllamaClient {
    config: OllamaConfig,
    http: reqwest::Client,
}

impl OllamaClient {
    pub fn new(config: OllamaConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(LlmError::from)?;
        Ok(Self { config, http })
    }

    fn resolved_base_url(&self) -> String {
        self.config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string()
    }
}

#[async_trait]
impl ModelClient for OllamaClient {
    fn name(&self) -> &str {
        "ollama"
    }

    fn provider_kind(&self) -> crate::ProviderKind {
        crate::ProviderKind::Ollama
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // 多数 Ollama 模型(qwen2.5 / llama3.1 / mistral-nemo / command-r)
            // 支持 tool call;小模型(llama3.2:1b / 旧 mistral / gemma:2b)不
            // 支持,reflect-core 已按 tool_use 过滤 LLM schema,错配时这些
            // 模型会返回 raw text 而非 tool_call。
            tool_use: true,
            // Ollama 不暴露 Anthropic/OpenAI 那种 cache hit 计费;
            // 模型的"留在显存里"是服务端决策,与 LLM token 计费无关。
            prompt_caching: false,
            // Ollama 无 `thinking` 字段;`ChatRequest.thinking` 透传时被忽略。
            extended_thinking: false,
            // vision 模型(llama3.2-vision / llava)支持图片,但 provider
            // 层静态 false;目前 reflect-tools 也没 image 工具,留 v0.4。
            vision: false,
            // Ollama 支持 `format: "json"` 强制 JSON 输出。
            json_mode: true,
            // Ollama 接受单字符串 system prompt,无多 block 概念。
            system_blocks: false,
        }
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let model = if request.model.is_empty() {
            self.config
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string())
        } else {
            request.model.clone()
        };
        let req = build_request(&request, &model, &self.config);

        let url = format!("{}/api/chat", self.resolved_base_url());

        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }

        // `api_key` 仅在显式配置时透传(Ollama Cloud / 反向代理);本地
        // `ollama serve` 不需要 Authorization header,留空即跳过。
        let mut request_builder = self.http.post(&url).json(&req);
        if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            request_builder = request_builder.header("Authorization", format!("Bearer {key}"));
        }

        let response = request_builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(classify_status(status_code, &text));
        }

        Ok(Box::pin(stream_ndjson(response, cancel)))
    }
}

// ── Status 分类 ─────────────────────────────────────────────────────────

fn classify_status(status: u16, body: &str) -> LlmError {
    match status {
        401 | 403 => LlmError::Auth,
        404 if body.contains("model") || body.contains("not found") => {
            // Ollama 在 model 不存在时返回 404;区别于 endpoint 404(后者也
            // 走同一分支,reflect 把它当作 model not found 也无害)。
            LlmError::InvalidRequest {
                message: format!("model not found: {body}"),
            }
        }
        400 if body.contains("context length") || body.contains("too long") => {
            LlmError::ContextLengthExceeded { used: 0, limit: 0 }
        }
        400 => LlmError::InvalidRequest {
            message: body.to_string(),
        },
        429 => LlmError::RateLimited {
            retry_after_ms: 1000,
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

// ── NDJSON → ChatEvent ──────────────────────────────────────────────────

fn stream_ndjson(
    response: Response,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ChatEvent, LlmError>> + Send {
    let byte_stream = response.bytes_stream();

    async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = byte_stream;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    yield Ok(ChatEvent::Error(LlmError::Cancelled));
                    return;
                }
                next = stream.next() => {
                    let Some(item) = next else {
                        // EOF:flush 缓冲区残留字节(可能最后一行无换行符)。
                        if !buf.is_empty() {
                            if let Some(evt) = parse_line(&buf) {
                                match evt {
                                    Ok(events) => {
                                        for ev in events {
                                            yield Ok(ev);
                                        }
                                    }
                                    Err(e) => {
                                        yield Err(e);
                                        return;
                                    }
                                }
                            }
                        }
                        return;
                    };
                    match item {
                        Ok(chunk) => {
                            buf.extend_from_slice(&chunk);
                            // 按 `\n` 切分;残余部分留在 buf 等下个 chunk。
                            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=pos).collect();
                                let line = &line[..line.len() - 1]; // 去掉 `\n`
                                if line.is_empty() {
                                    continue;
                                }
                                if let Some(evt) = parse_line(line) {
                                    match evt {
                                        Ok(events) => {
                                            for ev in events {
                                                yield Ok(ev);
                                            }
                                        }
                                        Err(e) => {
                                            yield Err(e);
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            yield Err(LlmError::Http(e.to_string()));
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// 解析一行 NDJSON(已剥去末尾 `\n`),返回该行产生的所有 `ChatEvent`。
///
/// - 成功:`Some(Ok(events))` —— 调用方负责把 vec 逐条 yield 出去。
/// - 解析失败:`Some(Err(SseParse))` —— 让 caller 中断流。
/// - 空行 / 纯空白心跳:`None` —— 跳过。
fn parse_line(line: &[u8]) -> Option<Result<Vec<ChatEvent>, LlmError>> {
    if line.is_empty() || line.iter().all(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let val: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return Some(Err(LlmError::SseParse(format!("invalid NDJSON: {e}")))),
    };
    let events = parse_chunk(&val);
    if events.is_empty() {
        return None;
    }
    Some(Ok(events))
}

fn parse_chunk(v: &Value) -> Vec<ChatEvent> {
    let mut out = Vec::new();
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");

    // 错误形态:`{"error": "..."}`(Ollama 在流中途失败时会发)。
    if let Some(err_msg) = v.get("error").and_then(|x| x.as_str()) {
        out.push(ChatEvent::Error(LlmError::Provider {
            status: 500,
            message: err_msg.to_string(),
        }));
        return out;
    }

    // 第一个有 model 字段的 chunk → MessageStart。
    if !model.is_empty() {
        out.push(ChatEvent::MessageStart {
            id: id.to_string(),
            model: model.to_string(),
        });
    }

    // `done: true` chunk 携带 usage 统计 + MessageStop。
    let done = v.get("done").and_then(|x| x.as_bool()).unwrap_or(false);
    if done {
        let input = v
            .get("prompt_eval_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let output = v.get("eval_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        out.push(ChatEvent::Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: 0,
            cache_write_tokens: 0,
        });
        out.push(ChatEvent::MessageStop);
        return out;
    }

    // 非 done chunk:从 `message` 字段读 assistant delta。
    if let Some(msg) = v.get("message") {
        // 文本增量
        if let Some(content) = msg.get("content").and_then(|x| x.as_str()) {
            if !content.is_empty() {
                out.push(ChatEvent::ContentDelta(content.to_string()));
            }
        }
        // tool calls (Ollama 原生格式:`{"function": {"name": ..., "arguments": {...}}}`)
        if let Some(tcs) = msg.get("tool_calls").and_then(|x| x.as_array()) {
            // 只取第一个 tool call —— Ollama 流里每个 chunk 一次只产一个。
            // 多 tool_call 的场景靠 reflect-core 后续 turn 重新触发。
            if let Some(tc) = tcs.first() {
                let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let args_val = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let args_str = serde_json::to_string(&args_val).unwrap_or_default();
                out.push(ChatEvent::ToolUseStart {
                    id: id.to_string(),
                    name: name.to_string(),
                    input_json: args_str,
                });
            }
        }
    }

    out
}

// ── ChatRequest → OllamaRequest ─────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct OllamaRequest {
    pub model: String,
    pub messages: Vec<Value>,
    pub stream: bool,
    /// `temperature` / `top_p` 在 Ollama 是顶层字段(不是 `options.*`)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// v1.4 B1:结构化输出。`JsonObject` → `"json"`(字符串);`JsonSchema`
    /// → schema 对象(Ollama 结构化输出支持把 JSON Schema 直接作为
    /// `format` 值)。原来是 `Option<String>`,为承载 schema 对象放宽为
    /// `Option<Value>`(wire 形态不变:None 时跳过序列化)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    /// Ollama 调参(`num_ctx` / `num_gpu` 等)放在 `options` 子对象。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

impl From<ChatRequest> for OllamaRequest {
    /// 公开供 test/snapshot 访问;生产路径走 `OllamaClient::stream`。
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
        // v1.4 B1:结构化输出 → Ollama `format` 字段。
        let format = match &req.response_format {
            Some(crate::request::ResponseFormat::JsonObject) => Some(serde_json::json!("json")),
            Some(crate::request::ResponseFormat::JsonSchema { schema, .. }) => Some(schema.clone()),
            // Text / None:纯文本,不设置 format。
            _ => None,
        };
        Self {
            model: req.model,
            messages,
            stream: true,
            temperature: req.temperature,
            top_p: req.top_p,
            format,
            options: None,
            keep_alive: None,
            tools,
            stop: req.stop,
        }
    }
}

/// 构造实际发送的 Ollama 请求,把 `OllamaConfig` 里的 num_ctx/num_gpu/
/// keep_alive 字段一并合并。`build_request` 与 `OllamaRequest::from`
/// 分开:前者处理"运行时拼接配置 + 解析 ChatRequest";后者只做 schema 转换。
fn build_request(req: &ChatRequest, model: &str, config: &OllamaConfig) -> OllamaRequest {
    let mut body = OllamaRequest::from(req.clone());
    body.model = model.to_string();
    if let Some(secs) = config.keep_alive_secs {
        body.keep_alive = Some(format_keep_alive(secs));
    }
    let mut options = serde_json::Map::new();
    if let Some(n) = config.num_ctx {
        options.insert("num_ctx".into(), Value::from(n));
    }
    if let Some(n) = config.num_gpu {
        options.insert("num_gpu".into(), Value::from(n));
    }
    if !options.is_empty() {
        body.options = Some(Value::Object(options));
    }
    body
}

/// Ollama `keep_alive` 字段接受字符串:
/// - `"5m"` / `"1h"` 等带单位格式
/// - 纯数字字符串 `"300"` 表示秒
/// - `"-1"` 表示永久
/// - `"0"` 表示立即卸载
///
/// 为简单起见,纯数字直接字符串化(负数加 `-`);用户也可以通过其它
/// 入口(直接配 config)传 `"5m"` 字面量,后续 v0.4 考虑支持字符串字段。
fn format_keep_alive(secs: i64) -> String {
    secs.to_string()
}

fn push_message(m: ChatMessage, out: &mut Vec<Value>) {
    match m {
        ChatMessage::System(s) => {
            out.push(serde_json::json!({"role": "system", "content": s}));
        }
        ChatMessage::User(u) => push_user(u, out),
        ChatMessage::Assistant(a) => {
            let mut msg = serde_json::json!({"role": "assistant"});
            if let Some(t) = a.text {
                if !t.is_empty() {
                    msg["content"] = Value::String(t);
                }
            }
            if !a.tool_calls.is_empty() {
                let tcs: Vec<Value> = a
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }
                        })
                    })
                    .collect();
                msg["tool_calls"] = Value::Array(tcs);
            }
            out.push(msg);
        }
        ChatMessage::Tool(t) => {
            // Ollama tool 角色 messages:`{"role": "tool", "content": "..."}`。
            // Ollama 不像 OpenAI 那样要求 `tool_call_id` 字段,匹配 content 靠
            // 上一轮 assistant 的 tool_call 顺序;reflect-core 维持顺序即可。
            // Ollama tool content 只接受字符串,用 content_as_text()(Image
            // 块占位,而非字节数组文本)。
            out.push(serde_json::json!({
                "role": "tool",
                "content": t.content_as_text(),
            }));
        }
    }
}

fn push_user(u: UserContent, out: &mut Vec<Value>) {
    let mut text = String::new();
    let mut images: Vec<String> = Vec::new();
    for b in u.blocks {
        match b {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            ContentBlock::Image { data, mime_type } => {
                // Ollama 接受 base64 图片数据(无 data URI 前缀)。
                images.push(base64_encode(&data));
                let _ = mime_type; // Ollama 不需要 mime_type
            }
        }
    }
    let mut msg = serde_json::json!({"role": "user", "content": text});
    if !images.is_empty() {
        msg["images"] = Value::Array(images.into_iter().map(Value::String).collect());
    }
    out.push(msg);
}

// 复用 OpenAI 的 base64 —— 内部独立实现避免跨 crate 公开。
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
    use crate::request::SystemBlocks;

    #[test]
    fn ollama_request_basic() {
        let req = ChatRequest {
            response_format: None,
            model: "llama3.2".into(),
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
        let body = OllamaRequest::from(req);
        let v: Value = serde_json::to_value(body).unwrap();
        assert_eq!(v["model"], "llama3.2");
        assert_eq!(v["stream"], true);
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn ollama_request_with_tools() {
        let req = ChatRequest {
            response_format: None,
            model: "qwen2.5:7b".into(),
            messages: vec![],
            tools: vec![ToolSpec::Function {
                name: "search".into(),
                description: "web search".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: Default::default(),
            stop: vec![],
        };
        let v: Value = serde_json::to_value(OllamaRequest::from(req)).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "search");
        assert_eq!(v["tools"][0]["function"]["description"], "web search");
    }

    #[test]
    fn build_request_with_keep_alive_zero_and_options() {
        let req = ChatRequest {
            response_format: None,
            model: "llama3.2".into(),
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
        let cfg = OllamaConfig {
            keep_alive_secs: Some(0),
            num_ctx: Some(4096),
            num_gpu: Some(35),
            ..Default::default()
        };
        let body = build_request(&req, "llama3.2", &cfg);
        let v: Value = serde_json::to_value(body).unwrap();
        assert_eq!(v["keep_alive"], "0");
        assert_eq!(v["options"]["num_ctx"], 4096);
        assert_eq!(v["options"]["num_gpu"], 35);
    }

    #[test]
    fn build_request_with_keep_alive_negative_one() {
        let req = ChatRequest::default();
        let cfg = OllamaConfig {
            keep_alive_secs: Some(-1),
            ..Default::default()
        };
        let body = build_request(&req, "llama3.2", &cfg);
        let v: Value = serde_json::to_value(body).unwrap();
        assert_eq!(v["keep_alive"], "-1");
    }

    #[test]
    fn parse_chunk_text_delta() {
        let chunk = serde_json::json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "Hello"},
            "done": false
        });
        let evs = parse_chunk(&chunk);
        assert!(
            evs.iter()
                .any(|e| matches!(e, ChatEvent::ContentDelta(s) if s == "Hello"))
        );
    }

    #[test]
    fn parse_chunk_tool_call() {
        let chunk = serde_json::json!({
            "model": "qwen2.5",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "search",
                        "arguments": {"query": "rust"}
                    }
                }]
            },
            "done": false
        });
        let evs = parse_chunk(&chunk);
        let tool_start = evs.iter().find_map(|e| match e {
            ChatEvent::ToolUseStart {
                name, input_json, ..
            } => Some((name.clone(), input_json.clone())),
            _ => None,
        });
        let (name, input_json) = tool_start.expect("expected ToolUseStart");
        assert_eq!(name, "search");
        // arguments 是 JSON object,序列化后是 object 字符串。
        let parsed: Value = serde_json::from_str(&input_json).unwrap();
        assert_eq!(parsed["query"], "rust");
    }

    #[test]
    fn parse_chunk_done_with_usage() {
        let chunk = serde_json::json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": ""},
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 42,
            "eval_count": 17,
            "total_duration": 1234567890u64
        });
        let evs = parse_chunk(&chunk);
        let usage = evs.iter().find_map(|e| match e {
            ChatEvent::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
                cache_write_tokens,
            } => Some((
                *input_tokens,
                *output_tokens,
                *cached_tokens,
                *cache_write_tokens,
            )),
            _ => None,
        });
        let (input, output, cached, write) = usage.expect("expected Usage event");
        assert_eq!(input, 42);
        assert_eq!(output, 17);
        assert_eq!(cached, 0);
        assert_eq!(write, 0);
        assert!(evs.iter().any(|e| matches!(e, ChatEvent::MessageStop)));
    }

    #[test]
    fn parse_chunk_error_message() {
        let chunk = serde_json::json!({
            "error": "model 'foo' not found"
        });
        let evs = parse_chunk(&chunk);
        let err = evs.iter().find_map(|e| match e {
            ChatEvent::Error(LlmError::Provider { status, message }) => {
                Some((*status, message.clone()))
            }
            _ => None,
        });
        let (status, message) = err.expect("expected Error(Provider)");
        assert_eq!(status, 500);
        assert!(message.contains("not found"));
    }

    #[test]
    fn classify_status_401() {
        let e = classify_status(401, "unauthorized");
        assert!(matches!(e, LlmError::Auth));
    }

    #[test]
    fn classify_status_404_model_not_found() {
        let e = classify_status(404, r#"{"error":"model \"foo\" not found"}"#);
        match e {
            LlmError::InvalidRequest { message } => {
                assert!(message.contains("model not found"));
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_500_internal() {
        let e = classify_status(500, "ollama internal");
        match e {
            LlmError::Provider { status, message } => {
                assert_eq!(status, 500);
                assert!(message.contains("ollama"));
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_context_length() {
        let e = classify_status(400, "context length exceeded");
        assert!(matches!(e, LlmError::ContextLengthExceeded { .. }));
    }

    #[test]
    fn capabilities_flags() {
        let client = OllamaClient::new(OllamaConfig::default()).unwrap();
        let c = client.capabilities();
        assert!(c.tool_use);
        assert!(c.json_mode);
        assert!(!c.prompt_caching);
        assert!(!c.extended_thinking);
        assert!(!c.vision);
        assert!(!c.system_blocks);
    }

    #[test]
    fn base64_roundtrip_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn parse_line_empty_returns_none() {
        assert!(parse_line(b"").is_none());
        assert!(parse_line(b"\n").is_none());
        assert!(parse_line(b"   ").is_none());
    }

    #[test]
    fn parse_line_invalid_json_returns_sse_parse_error() {
        let result = parse_line(b"not json at all");
        match result {
            Some(Err(LlmError::SseParse(_))) => {}
            other => panic!("expected SseParse error, got {other:?}"),
        }
    }

    #[test]
    fn resolved_base_url_trims_trailing_slash() {
        let cfg = OllamaConfig {
            base_url: Some("http://example.test:11434/".into()),
            ..Default::default()
        };
        let client = OllamaClient::new(cfg).unwrap();
        assert_eq!(client.resolved_base_url(), "http://example.test:11434");
    }

    #[test]
    fn resolved_base_url_default_when_none() {
        let client = OllamaClient::new(OllamaConfig::default()).unwrap();
        assert_eq!(client.resolved_base_url(), "http://127.0.0.1:11434");
    }

    /// v1.0.0-rc2：`OllamaClient` 重写 `provider_kind() = Ollama`。
    #[test]
    fn provider_kind_reports_ollama() {
        let client = OllamaClient::new(OllamaConfig::default()).unwrap();
        assert_eq!(client.provider_kind(), crate::ProviderKind::Ollama);
        assert_eq!(client.name(), "ollama");
    }
    // ── v1.4 B1:format 映射 ─────────────────────────────────────

    #[test]
    fn ollama_json_object_sets_format_json() {
        let req = ChatRequest {
            model: "llama3".into(),
            response_format: Some(crate::ResponseFormat::JsonObject),
            ..Default::default()
        };
        let wire = serde_json::to_value(OllamaRequest::from(req)).unwrap();
        assert_eq!(wire["format"], "json");
    }

    #[test]
    fn ollama_json_schema_sets_format_object() {
        let schema = serde_json::json!({"type": "object", "properties": {}});
        let req = ChatRequest {
            model: "llama3".into(),
            response_format: Some(crate::ResponseFormat::JsonSchema {
                name: "a".into(),
                schema: schema.clone(),
                strict: true,
            }),
            ..Default::default()
        };
        let wire = serde_json::to_value(OllamaRequest::from(req)).unwrap();
        assert_eq!(wire["format"], schema, "schema 对象应原样作为 format");
    }

    #[test]
    fn ollama_no_response_format_no_format() {
        let req = ChatRequest {
            model: "llama3".into(),
            ..Default::default()
        };
        let wire = serde_json::to_value(OllamaRequest::from(req)).unwrap();
        assert!(wire.get("format").is_none());
    }
}
