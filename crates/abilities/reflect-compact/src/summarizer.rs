//! LLM 驱动的对话摘要。
//!
//! 对应 reflect `summarizer.py` 的移植版。生成包含 9 个章节的中文摘要,
//! 用 `<summary>...</summary>` 标签包裹。在
//! [`crate::strategy::Compactor`] 中作为兜底策略:当 microcompact +
//! smart_prune 仍使对话超出预算时调用。
//!
//! 循环依赖规避:`Summarizer` trait 是 `Compactor` 依赖的抽象类型;
//! 具体实现 `LlmSummarizer` 虽位于本 crate,但只接收由调用方注入的
//! `Arc<dyn ModelClient>`。这意味着 `reflect-compact` 仅依赖
//! `reflect-llm` 的类型,而不依赖任何 provider 实现。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use reflect_llm::{
    ChatEvent, ChatMessage, ChatRequest, ContentBlock, ModelClient, Role, RoutingPolicy,
    SharedModelRegistry, SystemBlocks, ToolSpec, UserContent,
};
use reflect_telemetry::{ModelRef, SpanStatus, TelemetrySink, UsageSnapshot};

/// 单次摘要调用的默认超时,与 reflect 保持一致。
pub const SUMMARIZE_TIMEOUT: Duration = Duration::from_secs(90);

/// 9 段式中文 prompt。镜像 Reflect `summarizer.py` 的 SUMMARIZE_PROMPT。
pub const SUMMARIZE_PROMPT_FULL: &str = r#"你是一个专业的对话摘要助手。请分析以下对话历史,并按照以下9个部分生成结构化摘要。

## 输出格式
请用中文输出,使用以下9个章节(标题保持中文):

# 1. 主要请求和意图
# 2. 关键技术概念
# 3. 文件和代码部分
# 4. 错误和修复
# 5. 问题解决
# 6. 所有用户消息
# 7. 待处理任务
# 8. 当前工作
# 9. 可选的下一步

请将完整的摘要包裹在 <summary>...</summary> 标签中,例如:
<summary>
# 1. 主要请求和意图
...
</summary>

## 对话历史
{{ conversation }}
"#;

/// 增量摘要的 prompt(已存在历史摘要时使用)。
pub const SUMMARIZE_PROMPT_RECENT: &str = r#"你是一个专业的对话摘要助手。请基于之前的摘要和最近的新对话,生成更新后的摘要。

## 之前的摘要
<previous-summary>
{{ previous_summary }}
</previous-summary>

## 最近的对话
{{ recent_conversation }}

## 输出格式
请用中文输出9个章节(同之前格式),包裹在 <summary>...</summary> 标签中。
"#;

/// 摘要过程可能产生的错误。
#[derive(Debug, Error)]
pub enum SummarizerError {
    /// LLM 调用失败。
    #[error("summarizer llm error: {0}")]
    Llm(String),
    /// 流结束但未包含 `<summary>` 标签。
    #[error("summarizer output missing <summary> tag")]
    NoSummaryTag,
    /// 已被取消。
    #[error("summarizer cancelled")]
    Cancelled,
}

/// 摘要器抽象 trait。实现可使用任意后端(LLM、mock、固件文件等)。
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// 摘要完整对话。
    async fn summarize_full(&self, msgs: &[ChatMessage]) -> Result<String, SummarizerError>;

    /// 在已有旧摘要的前提下,摘要最近的对话切片。若实现不支持
    /// 增量模式,可回退到 `summarize_full`。
    async fn summarize_recent(
        &self,
        msgs: &[ChatMessage],
        previous_summary: Option<&str>,
    ) -> Result<String, SummarizerError>;
}

/// 由 LLM 驱动的 summarizer。流式读取模型响应,拼接
/// `ContentDelta` 事件,并解析出 `<summary>...</summary>`。
///
/// v1.0 多 Provider 路由:持 `SharedModelRegistry` + `Arc<RoutingPolicy>`,
/// `call_summarizer` 内部走 `Role::Compact` slot,失败时由
/// `ModelRegistry::next_for` 在 pool 内自动切下一个 credential。
pub struct LlmSummarizer {
    registry: SharedModelRegistry,
    policy: Arc<RoutingPolicy>,
    /// 初始 spec 起点(由 `Role::Compact` slot primary 提供)。
    model_name: String,
    timeout: Duration,
    /// v1.2 P1:可选 telemetry sink,注入后每次摘要调用落 model-io 记录
    /// (query_source = "compact")。`None`(默认)= 不落库,向后兼容。
    telemetry: Option<Arc<TelemetrySink>>,
}

impl std::fmt::Debug for LlmSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSummarizer")
            .field("model_name", &self.model_name)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl LlmSummarizer {
    /// v1.0 多 Provider 路由:`registry` + `policy` 注入,初次调用按
    /// `Role::Compact` slot primary 派位 client。失败时由 registry
    /// 自动在 pool 内切换下一个 credential。
    pub fn new(
        registry: SharedModelRegistry,
        policy: Arc<RoutingPolicy>,
        model_name: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            policy,
            model_name: model_name.into(),
            timeout: SUMMARIZE_TIMEOUT,
            telemetry: None,
        }
    }

    /// v1.2 P1:注入 telemetry sink。返回 `Self` 供链式调用,不破坏 `new()`
    /// 既有签名。`reflect-exec::bootstrap` 构造 summarizer 后链式调用。
    pub fn with_telemetry(mut self, sink: Option<Arc<TelemetrySink>>) -> Self {
        self.telemetry = sink;
        self
    }
}

#[async_trait]
impl Summarizer for LlmSummarizer {
    async fn summarize_full(&self, msgs: &[ChatMessage]) -> Result<String, SummarizerError> {
        let conversation = serialize_messages(msgs);
        let prompt = SUMMARIZE_PROMPT_FULL.replace("{{ conversation }}", &conversation);
        call_summarizer(self, &prompt, None).await
    }

    async fn summarize_recent(
        &self,
        msgs: &[ChatMessage],
        previous_summary: Option<&str>,
    ) -> Result<String, SummarizerError> {
        let recent = serialize_messages(msgs);
        let prompt = match previous_summary {
            Some(prev) if !prev.is_empty() => SUMMARIZE_PROMPT_RECENT
                .replace("{{ previous_summary }}", prev)
                .replace("{{ recent_conversation }}", &recent),
            _ => {
                // 无先前摘要 → 行为等同 full。
                let full = SUMMARIZE_PROMPT_FULL.replace("{{ conversation }}", &recent);
                return call_summarizer(self, &full, None).await;
            }
        };
        call_summarizer(self, &prompt, Some(previous_summary.unwrap_or(""))).await
    }
}

async fn call_summarizer(
    s: &LlmSummarizer,
    prompt: &str,
    _previous: Option<&str>,
) -> Result<String, SummarizerError> {
    let request = ChatRequest {
        model: s.model_name.clone(),
        system: SystemBlocks::default(),
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text(prompt)],
        })],
        tools: vec![ToolSpec::Function {
            name: "noop".into(),
            description: "no-op tool to satisfy tool-use-only providers".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }],
        ..Default::default()
    };
    let cancel = CancellationToken::new();

    // v1.0 多 Provider 路由:从 `Role::Compact` slot 拿初始 spec,
    // 失败时由 `next_for` 在 pool 内切下一个 credential。简化版的
    // failover 循环:`max_attempts = candidates * 2` 之内反复试,
    // 速率限制 / Auth / 5xx 直接 mark_cooldown 切下一个。
    let initial_spec = s.policy.resolve(Role::Compact).primary.clone();
    let spec = if initial_spec.is_empty() {
        s.model_name.clone()
    } else {
        initial_spec
    };
    let mut exclude: Vec<Arc<dyn ModelClient>> = Vec::new();
    let max_attempts = s.policy.max_attempts;
    let mut attempt: u32 = 0;
    // v1.2 P1:整体调用计时(从首次尝试到流消费完成)。
    let call_started = std::time::Instant::now();
    // 记录成功 break 出来的凭证信息(供落库 query_source/metadata)。
    let (stream, succ_provider, succ_label, succ_model): (_, String, String, String) = loop {
        attempt += 1;
        if attempt > max_attempts {
            return Err(SummarizerError::Llm("all credentials exhausted".into()));
        }
        let nc = match s.registry.next_for(&spec, &exclude) {
            Some(nc) => nc,
            None => return Err(SummarizerError::Llm("no credential available".into())),
        };
        let client = nc.client.clone();
        let label = nc.label.clone();
        let provider = client.name().to_string();
        let model_name = spec
            .split_once('/')
            .map(|(_, m)| m)
            .unwrap_or(&spec)
            .to_string();
        let req = ChatRequest {
            // 剥掉 "provider/" 前缀，符合 `ChatRequest.model` 纯模型名契约，
            // 避免前缀泄漏到上游 API 请求体（与 reflect-core model_call 一致）。
            model: model_name.clone(),
            ..request.clone()
        };
        let stream_fut = client.stream(req, cancel.clone());
        match timeout(s.timeout, stream_fut).await {
            Ok(Ok(stream)) => {
                s.registry.clear_cooldown(&provider, &label);
                break (stream, provider, label, model_name);
            }
            Ok(Err(e)) => {
                use reflect_llm::{CooldownReason, LlmError};
                let cooldown = match &e {
                    LlmError::Auth => Duration::from_secs(3600),
                    LlmError::RateLimited { retry_after_ms } => {
                        Duration::from_millis(*retry_after_ms)
                            .max(s.policy.default_cooldown_rate_limited)
                    }
                    LlmError::Overloaded { retry_after_ms } => {
                        Duration::from_millis(*retry_after_ms)
                    }
                    LlmError::Provider { status, .. } if *status >= 500 => Duration::from_secs(60),
                    LlmError::ContextLengthExceeded { .. } | LlmError::InvalidRequest { .. } => {
                        // 客户端错误,不重试
                        return Err(SummarizerError::Llm(e.to_string()));
                    }
                    _ => Duration::from_secs(0), // 瞬时网络/解析错误,试下一个
                };
                if !cooldown.is_zero() {
                    let reason = match &e {
                        LlmError::Auth => CooldownReason::Auth,
                        LlmError::RateLimited { retry_after_ms } => CooldownReason::RateLimited {
                            retry_after_ms: *retry_after_ms,
                        },
                        LlmError::Overloaded { .. } => CooldownReason::Overloaded,
                        LlmError::Provider { status, .. } => {
                            CooldownReason::Provider5xx { status: *status }
                        }
                        _ => CooldownReason::Auth,
                    };
                    s.registry
                        .mark_cooldown(&provider, &label, cooldown, reason);
                }
                exclude.push(client);
            }
            Err(_) => {
                // 单次 timeout → 切下一个 credential,不 mark_cooldown
                // (timeout 不代表 credential 不可用,可能只是网络抖动)。
                exclude.push(client);
            }
        }
    };
    let mut s_pin = std::pin::pin!(stream);
    let mut buf = String::new();
    let mut cancelled = false;
    // v1.2 P1:累积 token usage(此前 `_ => {}` 把 Usage 事件丢了)。
    let mut usage_input: u64 = 0;
    let mut usage_output: u64 = 0;
    let mut usage_cached: u64 = 0;
    let mut usage_cache_write: u64 = 0;
    loop {
        let evt = match s_pin.next().await {
            Some(Ok(e)) => e,
            Some(Err(e)) => {
                if cancelled {
                    return Err(SummarizerError::Cancelled);
                }
                return Err(SummarizerError::Llm(e.to_string()));
            }
            None => break,
        };
        if cancel.is_cancelled() {
            cancelled = true;
            continue;
        }
        match evt {
            ChatEvent::ContentDelta(d) => buf.push_str(&d),
            ChatEvent::ThinkingDelta(d) => buf.push_str(&d), // tolerate
            ChatEvent::MessageStop => break,
            ChatEvent::Error(e) => return Err(SummarizerError::Llm(e.to_string())),
            ChatEvent::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
                cache_write_tokens,
            } => {
                usage_input = input_tokens as u64;
                usage_output = output_tokens as u64;
                usage_cached = cached_tokens as u64;
                usage_cache_write = cache_write_tokens as u64;
            }
            _ => {}
        }
    }
    if cancelled {
        return Err(SummarizerError::Cancelled);
    }
    // v1.2 P1:落库本次摘要调用(若有 sink)。完整 prompt 已序列化进 request,
    // 响应是 summary 全文(buf)。cost 由 pricing 表估算。
    if let Some(sink) = s.telemetry.as_ref()
        && sink.enabled()
    {
        let cost_usd = reflect_llm::price(
            &succ_model,
            &reflect_protocol::TokenUsage {
                input_tokens: usage_input as u32,
                output_tokens: usage_output as u32,
                cached_tokens: usage_cached as u32,
                cache_write_tokens: usage_cache_write as u32,
                total_tokens: (usage_input + usage_output) as u32,
            },
        );
        let model_ref = ModelRef {
            model_id: succ_model.clone(),
            provider_id: Some(succ_provider.clone()),
            role: Some("compact".into()),
            source: Some("compact".into()),
        };
        let req_record = serde_json::json!({
            "model": spec,
            "attempt": attempt,
            "provider": succ_provider,
            "credential_label": succ_label,
            "messages": serde_json::to_value(&request.messages).unwrap_or(serde_json::Value::Null),
            "system": serde_json::to_value(&request.system).unwrap_or(serde_json::Value::Null),
        });
        let resp_record = serde_json::json!({
            "finish_reason": "stop",
            "text": buf,
        });
        sink.record_model_call(
            None,
            None,
            model_ref,
            req_record,
            resp_record,
            UsageSnapshot {
                input_tokens: usage_input,
                output_tokens: usage_output,
                cached_tokens: usage_cached,
                cache_write_tokens: usage_cache_write,
                total_tokens: usage_input + usage_output,
                cost_usd,
            },
            call_started.elapsed().as_millis() as u64,
            attempt,
            SpanStatus::Completed,
            "compact",
        );
    }
    parse_summary(&buf).ok_or(SummarizerError::NoSummaryTag)
}

/// 把消息序列化为 LLM 可读的纯文本 transcript。
/// 上限约 200k 字符,防病态输入。
pub fn serialize_messages(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    for m in messages {
        let s = match m {
            ChatMessage::System(s) => format!("[System]\n{s}\n"),
            ChatMessage::User(u) => {
                let mut s = String::from("[User]\n");
                for b in &u.blocks {
                    match b {
                        ContentBlock::Text { text } => s.push_str(text),
                        ContentBlock::Image { .. } => s.push_str("[image]"),
                    }
                }
                s.push('\n');
                s
            }
            ChatMessage::Assistant(a) => {
                let mut s = String::from("[Assistant]\n");
                if let Some(text) = &a.text {
                    s.push_str(text);
                }
                if !a.tool_calls.is_empty() {
                    s.push_str(&format!(
                        "\n[Tools: {}]\n",
                        a.tool_calls
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                s.push('\n');
                s
            }
            ChatMessage::Tool(t) => format!("[Tool({})]\n{}\n", t.call_id, t.content_as_text()),
        };
        if total + s.len() > 200_000 {
            out.push_str("...[truncated for length]...");
            break;
        }
        out.push_str(&s);
        out.push_str("\n---\n");
        total += s.len();
    }
    out
}

/// 从 LLM 原始输出中提取 `<summary>...</summary>` 内容。
/// 无该标签时返回 `None`。
pub fn parse_summary(raw: &str) -> Option<String> {
    let start = raw.find("<summary>")?;
    let after = start + "<summary>".len();
    let end = raw[after..].find("</summary>")?;
    Some(raw[after..after + end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::AssistantContent;

    #[test]
    fn parse_summary_extracts_content() {
        let raw = "<analysis>blah</analysis><summary>hello world</summary>extra";
        assert_eq!(parse_summary(raw).as_deref(), Some("hello world"));
    }

    #[test]
    fn parse_summary_trims_whitespace() {
        let raw = "<summary>\n  hello\n  world  \n</summary>";
        assert_eq!(parse_summary(raw).as_deref(), Some("hello\n  world"));
    }

    #[test]
    fn parse_summary_returns_none_when_no_tag() {
        assert!(parse_summary("no tags here").is_none());
    }

    #[test]
    fn parse_summary_returns_none_when_unclosed() {
        assert!(parse_summary("<summary>unfinished").is_none());
    }

    #[test]
    fn serialize_messages_includes_all_roles() {
        let msgs = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            }),
            ChatMessage::Assistant(AssistantContent {
                text: Some("hello".into()),
                tool_calls: vec![],
                thinking: None,
            }),
            ChatMessage::Tool(reflect_llm::ToolResult {
                call_id: "c1".into(),
                content: vec![reflect_llm::ContentBlock::text("ok")],
                is_error: false,
            }),
        ];
        let s = serialize_messages(&msgs);
        assert!(s.contains("[System]"));
        assert!(s.contains("[User]"));
        assert!(s.contains("[Assistant]"));
        assert!(s.contains("[Tool(c1)]"));
    }

    #[test]
    fn serialize_messages_truncates_at_200k() {
        let huge = "x".repeat(300_000);
        let msgs = vec![ChatMessage::System(huge)];
        let s = serialize_messages(&msgs);
        assert!(s.contains("[truncated for length]"));
        assert!(s.len() < 210_000);
    }

    #[test]
    fn summarize_prompts_have_placeholders() {
        assert!(SUMMARIZE_PROMPT_FULL.contains("{{ conversation }}"));
        assert!(SUMMARIZE_PROMPT_RECENT.contains("{{ previous_summary }}"));
        assert!(SUMMARIZE_PROMPT_RECENT.contains("{{ recent_conversation }}"));
    }
}
