//! 流式消费循环 — tokio::select! 主循环消费 ChatEvent 流。
//!
//! - `process_stream_events` — 从 `&mut dyn Stream` 消费事件
//! - `process_midstream_error` — 流中错误的 cooldown/failover 决策

use futures::{Stream, StreamExt};
use reflect_llm::{ChatEvent, LlmError};
use reflect_protocol::{
    AbortReason, AgentMessageDelta, ErrorEvent, Event, EventMsg, StreamErrorEvent, ThinkingDelta,
    TokenUsage, ToolCallBeginEvent, TriedCredential, TurnAbortedEvent,
};
use tokio::time::sleep;

use super::super::retry::{RetryAction, classify_action, error_code, outcome_code};
use crate::submission_loop::NodeContext;

/// 结束原因之后等待尾部 usage chunk 的上限。
///
/// provider 把 usage 放在「结束原因之后」的额外 chunk 里是常见做法
/// (OpenAI `stream_options.include_usage`、Kilo 免费池实测均如此)。
/// 该 chunk 通常紧随其后到达,但有些 provider 收尾后不立刻关连接 ——
/// 用短超时兜底,避免整个回合被拖住。
const USAGE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);

/// 解析流式累积的工具参数 JSON。
///
/// 模型(尤其弱模型)可能输出空 / 截断 / 非法 JSON,导致 `from_str` 失败。
/// 此前用 `unwrap_or(Value::Null)` 静默退化,使得下游工具(如 bash)拿到
/// `Value::Null` 后报 "missing 'cmd'" —— 难以定位根因。这里在解析失败时
/// 记一条 `warn!`(工具名 + 累积长度 + 解析错误),保留同样的 `Null` 退化
/// 行为(零行为变更),让该失效模式可观测。
fn parse_tool_args(args_str: &str, tool_name: &str) -> serde_json::Value {
    serde_json::from_str(args_str).unwrap_or_else(|e| {
        tracing::warn!(
            tool = tool_name,
            args_len = args_str.len(),
            error = %e,
            "tool args JSON parse failed; falling back to Value::Null \
             (likely empty/truncated/malformed streamed arguments from the model)"
        );
        serde_json::Value::Null
    })
}

/// 流式消费返回值,与 `model_call` loop 的 `break` 元组对齐。
pub type StreamResult = (
    String,                                   // text_buf
    Vec<(String, String, serde_json::Value)>, // tool_calls
    TokenUsage,                               // usage
    String,                                   // provider
    String,                                   // label
    bool,                                     // truncated
    String,                                   // thinking_buf
);

/// 从 `client.stream()` 消费事件直到流结束。
///
/// `s` 是 `std::pin::pin!(stream_result?)` 的结果,类型为
/// `Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>`。
/// 由于 `Box` 是 `Unpin`,可以直接 `&mut *s` 传引用。
///
/// - `Ok(StreamResult)` — 流正常结束
/// - `Err(RetryAction)` — 流中发生错误,上层根据决策处理
pub async fn process_stream_events(
    s: &mut (dyn Stream<Item = std::result::Result<ChatEvent, LlmError>> + Send + Unpin),
    tried: &mut Vec<TriedCredential>,
    provider_name: &str,
    label: &str,
    ctx: &NodeContext,
    default_cooldown_rate_limited: std::time::Duration,
) -> Result<StreamResult, RetryAction> {
    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    let mut tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
    let mut current_tool: Option<(String, String)> = None;
    let mut current_args_str = String::new();
    // 本轮工具的参数是**从 ToolUseStart 带过来的**(`input_json` 非空)吗?
    //
    // provider 的两种参数投递形态:
    // - **分片形态**:ToolUseStart 带**空** `input_json`,参数随后由若干
    //   `ToolUseDelta` 分片送来(Anthropic / OpenAI / Responses)。
    // - **整包形态**:ToolUseStart 直接带**完整** `input_json`,**没有**
    //   delta(Ollama 原生格式;mock provider 也发整包 + 一份重复 delta)。
    //
    // 此前只累积 delta、把 `input_json` 整个忽略 —— 于是**整包形态的
    // provider 参数全丢**(工具收到 `args = null`,表现为「missing 'cmd'」
    // 这类假故障)。本标记用来区分两种形态:整包形态下忽略随后的 delta
    // (mock 会重复发一份),分片形态下正常累积。
    let mut args_from_start = false;
    let mut usage = TokenUsage::default();
    let mut stop = false;
    // 收到结束原因后的「排空」阶段:只再收 usage 事件。
    //
    // 某些 provider 把 usage 放在**结束原因之后**的额外 chunk 里 ——
    // OpenAI(`stream_options.include_usage`)与 Kilo 免费池实测均如此。
    // 此前在 `MessageStop` 处直接 break,尾部的 usage chunk 被丢弃 →
    // `token_count` 恒为 0,配额/成本统计全部失真。
    let mut draining = false;
    // 排空阶段的截止时刻(进入排空时设定)—— 整个排空窗口有界。
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    let mut error: Option<LlmError> = None;
    let mut truncated = false;

    loop {
        // 排空阶段:在有界窗口内继续读,**跳过非 usage 事件**直到拿到 usage。
        //
        // 为什么不能只读一个事件:provider 的每个 chunk 都会先产出一个
        // `MessageStart`(OpenAI 兼容形状里 id/model 每帧都有),所以尾部
        // usage chunk 前面还隔着一个 `MessageStart` —— 只读一次拿到的是
        // 它,usage 反而被漏掉。
        if draining {
            let deadline = drain_deadline.unwrap_or_else(tokio::time::Instant::now);
            match tokio::time::timeout_at(deadline, s.next()).await {
                Ok(Some(Ok(ChatEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens,
                }))) => {
                    usage = TokenUsage {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        cache_write_tokens,
                        total_tokens: input_tokens + output_tokens,
                    };
                    break;
                }
                // 非 usage 事件(如每个 chunk 都带的 MessageStart)——跳过继续等。
                Ok(Some(Ok(_))) => continue,
                // 流结束 / 出错 / 超时 —— 放弃排空。
                _ => break,
            }
        }
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                let _ = ctx.event_tx.send(Event::new(
                    ctx.sub_id.clone(),
                    EventMsg::TurnAborted(TurnAbortedEvent {
                        turn_id: ctx.turn_id,
                        reason: AbortReason::UserInterrupt,
                    }),
                )).await;
                return Err(RetryAction::GiveUp);
            }
            evt = s.next() => {
                match evt {
                    Some(Ok(ChatEvent::ContentDelta(d))) => {
                        text_buf.push_str(&d);
                        let _ = ctx.event_tx.send(Event::new(
                            ctx.sub_id.clone(),
                            EventMsg::AgentMessageDelta(AgentMessageDelta { delta: d }),
                        )).await;
                    }
                    Some(Ok(ChatEvent::ThinkingDelta(d))) => {
                        // 转发 thinking delta 给 TUI(kind="raw" = token 级流式)。
                        // 此前该变体落入下方通配符 `Some(Ok(_)) => {}` 被静默丢弃,
                        // 导致 TUI 的 thinking 渲染(cell/adapter 已就绪)永远收不到数据。
                        thinking_buf.push_str(&d);
                        let _ = ctx.event_tx.send(Event::new(
                            ctx.sub_id.clone(),
                            EventMsg::ThinkingDelta(ThinkingDelta {
                                delta: d,
                                kind: "raw".to_string(),
                            }),
                        )).await;
                    }
                    Some(Ok(ChatEvent::ToolUseStart { id, name, input_json })) => {
                        let _ = ctx.event_tx.send(Event::new(
                            ctx.sub_id.clone(),
                            EventMsg::ToolCallBegin(ToolCallBeginEvent {
                                call_id: id.clone(),
                                tool_name: name.clone(),
                                args: serde_json::Value::Null,
                                child_id: None,
                            }),
                        )).await;
                        // 一个 assistant turn 可能并行发起多个工具调用
                        // (Anthropic / OpenAI 均支持)。遇到下一个 ToolUseStart
                        // 时,先把上一个工具累积的 (id, name, args) 落盘 ——
                        // 否则会被下面 clear() / 覆盖静默丢弃,导致只有最后一个
                        // 工具被记录、其余工具的 tool_use id 在历史里孤立
                        // (无配对 tool_result → provider 400)。
                        if let Some((prev_id, prev_name)) = current_tool.take() {
                            let prev_args = parse_tool_args(&current_args_str, &prev_name);
                            tool_calls.push((prev_id, prev_name, prev_args));
                        }
                        current_tool = Some((id, name));
                        // 播种而非清空:整包形态的 provider(Ollama 原生
                        // 格式)把**完整**参数放在这里且**不发** delta ——
                        // 清空会把参数整个丢掉。
                        args_from_start = !input_json.is_empty();
                        current_args_str = input_json;
                    }
                    Some(Ok(ChatEvent::ToolUseDelta(partial))) => {
                        // 整包形态:参数已由 ToolUseStart 带全,随后的 delta
                        // 是重复的(mock provider 就这么发)—— 忽略,否则会
                        // 拼成两遍 JSON,解析失败又变回 `args = null`。
                        if !args_from_start {
                            current_args_str.push_str(&partial);
                        }
                    }
                    Some(Ok(ChatEvent::MessageStopTruncated { .. })) => {
                        truncated = true;
                        stop = true;
                    }
                    Some(Ok(ChatEvent::MessageStop)) | None => { stop = true; }
                    Some(Ok(ChatEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        cache_write_tokens,
                    })) => {
                        usage = TokenUsage {
                            input_tokens,
                            output_tokens,
                            cached_tokens,
                            cache_write_tokens,
                            total_tokens: input_tokens + output_tokens,
                        };
                    }
                    Some(Ok(ChatEvent::Error(e))) => { error = Some(e); stop = true; }
                    Some(Err(e)) => { error = Some(e); stop = true; }
                    Some(Ok(_)) => {}
                }
                if stop {
                    // 流中出错:立即结束,不排空(错误优先)。
                    if error.is_some() {
                        break;
                    }
                    // 结束原因已到 —— 进入排空阶段等尾部 usage chunk。
                    draining = true;
                    drain_deadline =
                        Some(tokio::time::Instant::now() + USAGE_DRAIN_TIMEOUT);
                }
            }
        }
    }

    if let Some(e) = error {
        // Flush pending tool call before deciding retry.
        if let Some((id, name)) = current_tool.take() {
            let args = parse_tool_args(&current_args_str, &name);
            tool_calls.push((id, name, args));
        }
        return Err(process_midstream_error(
            &e,
            provider_name,
            label,
            !text_buf.is_empty() || !tool_calls.is_empty(),
            tried,
            ctx,
            default_cooldown_rate_limited,
        )
        .await);
    }

    // Flush pending tool call.
    if let Some((id, name)) = current_tool.take() {
        let args = parse_tool_args(&current_args_str, &name);
        tool_calls.push((id, name, args));
    }

    Ok((
        text_buf,
        tool_calls,
        usage,
        provider_name.to_string(),
        label.to_string(),
        truncated,
        thinking_buf,
    ))
}

/// 流式消费中发生错误时的处理逻辑:分类 + cooldown + 事件 + 决策。
async fn process_midstream_error(
    e: &LlmError,
    provider_name: &str,
    label: &str,
    succeeded: bool,
    tried: &mut Vec<TriedCredential>,
    ctx: &NodeContext,
    default_cooldown_rate_limited: std::time::Duration,
) -> RetryAction {
    let action = classify_action(e, default_cooldown_rate_limited);
    let outcome = outcome_code(e).to_string();

    match action {
        RetryAction::CooldownAndFailover { cooldown } => {
            let reason = build_cooldown_reason(e);
            ctx.registry
                .mark_cooldown(provider_name, label, cooldown, reason);
            let _ = ctx
                .event_tx
                .send(Event::new(
                    ctx.sub_id.clone(),
                    EventMsg::StreamError(StreamErrorEvent {
                        code: error_code(e).into(),
                        message: e.to_string(),
                        retry_in_ms: cooldown.as_millis() as u64,
                        provider: Some(provider_name.to_string()),
                        credential_label: Some(label.to_string()),
                        tried: Some({
                            let mut t = tried.clone();
                            t.push(TriedCredential {
                                label: label.to_string(),
                                outcome: outcome.clone(),
                            });
                            t
                        }),
                    }),
                ))
                .await;
        }
        RetryAction::RetrySame { delay_ms } => {
            if !succeeded {
                sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        RetryAction::Failover => {}
        RetryAction::GiveUp => {
            tried.push(TriedCredential {
                label: label.to_string(),
                outcome: outcome.clone(),
            });
            let _ = ctx
                .event_tx
                .send(Event::new(
                    ctx.sub_id.clone(),
                    EventMsg::Error(ErrorEvent {
                        code: error_code(e).into(),
                        message: e.to_string(),
                        details: Some(serde_json::json!({
                            "provider": provider_name,
                            "credential_label": label,
                            "tried": tried.clone(),
                        })),
                    }),
                ))
                .await;
            return RetryAction::GiveUp;
        }
    }

    tried.push(TriedCredential {
        label: label.to_string(),
        outcome,
    });
    action
}

/// 根据错误类型构造对应的 `CooldownReason`。
fn build_cooldown_reason(e: &LlmError) -> reflect_llm::CooldownReason {
    match e {
        LlmError::RateLimited { .. } => {
            reflect_llm::CooldownReason::RateLimited { retry_after_ms: 0 }
        }
        LlmError::Overloaded { .. } => reflect_llm::CooldownReason::Overloaded,
        LlmError::Provider { status, .. } => {
            reflect_llm::CooldownReason::Provider5xx { status: *status }
        }
        LlmError::Auth => reflect_llm::CooldownReason::Auth,
        _ => reflect_llm::CooldownReason::Auth,
    }
}
