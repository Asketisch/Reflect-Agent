//! `model_call` 节点 —— 调用 LLM 并流式接收其响应。
//!
//! 拆分子模块:
//! - [`stream`] — tokio::select! 流式消费循环
//!
//! 重试决策逻辑(需要 mutable `exclude`)保留在 mod.rs 的 loop 内。

mod stream;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reflect_llm::{AssistantContent, ChatMessage, ChatRequest, CooldownReason, LlmError, Role};
use reflect_protocol::{
    ContentBlock, ErrorEvent, Event, EventMsg, RolloutRecord, RoutingEvent, RoutingEventKind,
    StreamErrorEvent, TokenCountEvent, TokenUsage, TriedCredential,
};
use tokio::time::sleep;

use super::nudge::MAX_AUTO_CONTINUATIONS;
use super::retry::{RetryAction, classify_action, error_code, outcome_code};
use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;
use reflect_recovery::recovery_meta_to_messages;

use crate::graph::nodes::model_call::stream::process_stream_events;

/// `model_call` —— 调用 LLM 并流式返回响应。
pub async fn model_call(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    state.iteration = state.iteration.saturating_add(1);
    if state.iteration > ctx.max_iterations {
        if state.force_final_answer {
            state.completed_normally = true;
            state.hit_max_iterations = true;
            return None;
        }
        state.force_final_answer = true;
        state.hit_max_iterations = true;
        state.iteration = ctx.max_iterations;
        state
            .messages
            .messages
            .push(ChatMessage::User(reflect_llm::UserContent {
                blocks: vec![reflect_llm::ContentBlock::Text {
                    text: format!(
                        "<system-reminder>\nYou have reached the maximum number of \
                           tool-use steps for this task. You can no longer use any tools. Based \
                           ONLY on the information you have already gathered, give your final \
                           answer right now using the template: {}. Do not say you cannot answer \
                           — provide your best answer.\n</system-reminder>",
                        reflect_prompt::FINAL_ANSWER_TEMPLATE
                    ),
                }],
            }));
        tracing::info!(
            iteration = state.iteration,
            max_iterations = ctx.max_iterations,
            "max_iterations reached; forcing one tool-less final-answer call"
        );
    }

    let model = ctx.model.read().clone();
    let initial_spec = ctx.policy.resolve(Role::Main).primary.clone();
    let spec = if initial_spec.is_empty() {
        model.clone()
    } else {
        initial_spec
    };

    let mut messages = if !state.messages.messages.is_empty() {
        state.messages.messages.clone()
    } else {
        ctx.messages.clone()
    };
    if !state.ephemeral_text.trim().is_empty() {
        messages.push(ChatMessage::User(reflect_llm::UserContent {
            blocks: vec![reflect_llm::ContentBlock::Text {
                text: format!(
                    "<system-reminder>\n{}\n</system-reminder>",
                    state.ephemeral_text
                ),
            }],
        }));
    }
    messages.extend(recovery_meta_to_messages(&state.recovery_meta));
    let tools: Vec<reflect_llm::ToolSpec> = if state.force_final_answer {
        Vec::new()
    } else if !state.effective_tools.is_empty() {
        state.effective_tools.clone()
    } else {
        ctx.tools_queue
            .registry()
            .list_specs()
            .into_iter()
            .map(|s| match s {
                reflect_tools::ToolSpec::Function {
                    name,
                    description,
                    parameters,
                    ..
                } => reflect_llm::ToolSpec::Function {
                    name: name.clone(),
                    description: description.clone(),
                    parameters: parameters.clone(),
                },
            })
            .collect()
    };
    let mut request = ChatRequest {
        model: spec
            .split_once('/')
            .map(|(_, m)| m)
            .unwrap_or(&spec)
            .to_string(),
        messages,
        tools,
        system: state.system_blocks.clone(),
        thinking: Some(reflect_llm::ThinkingConfig::OpenAIReasoning {
            effort: (*ctx.effort.read()).into(),
        }),
        ..Default::default()
    };

    // M4:注入 cache_control 断点
    if let Some(m4) = ctx.m4.as_ref() {
        let caps = ctx
            .registry
            .resolve(&spec)
            .map(|c| c.capabilities())
            .unwrap_or_default();
        let mut builder = m4.prompt_builder.lock();
        let _ = builder.build_request(
            &reflect_prompt::LayeredPrompt::new(),
            request.messages.clone(),
            request.tools.clone(),
            request.model.clone(),
            &caps,
        );
        if caps.prompt_caching {
            let _ = reflect_prompt::inject_cache_control(&mut request, &caps);
        }
    }

    // v1.0 多 Provider 路由:池轮询 retry loop。
    let mut exclude: Vec<Arc<dyn reflect_llm::ModelClient>> = Vec::new();
    let mut tried: Vec<TriedCredential> = Vec::new();
    let mut per_cred_attempts: HashMap<usize, u32> = HashMap::new();
    let mut attempt: u32 = 0;
    let max_attempts = ctx.policy.max_attempts;
    let default_cooldown_rate_limited = ctx.policy.default_cooldown_rate_limited;
    let llm_call_started = std::time::Instant::now();

    let (text_buf, tool_calls, usage, success_provider, success_label, truncated, thinking_buf) = loop {
        let next_client = match ctx.registry.next_for(&spec, &exclude) {
            Some(nc) => nc,
            None => {
                failed(tried.clone(), ctx, "ALL_CREDENTIALS_EXHAUSTED").await;
                return None;
            }
        };
        let client: Arc<dyn reflect_llm::ModelClient> = next_client.client.clone();
        let label: String = next_client.label.clone();
        let provider_name: String = client.name().to_string();
        let cred_ptr = Arc::as_ptr(&client) as *const () as usize;

        attempt += 1;
        if attempt > max_attempts {
            tried.push(TriedCredential {
                label: label.clone(),
                outcome: "max_attempts".into(),
            });
            failed(tried.clone(), ctx, "MAX_ATTEMPTS").await;
            return None;
        }
        let cred_attempt = per_cred_attempts.entry(cred_ptr).or_insert(0);
        *cred_attempt += 1;

        let stream_result = client.stream(request.clone(), ctx.cancel.clone()).await;
        let mut s = match stream_result {
            Ok(s) => {
                ctx.registry.clear_cooldown(&provider_name, &label);
                std::pin::pin!(s)
            }
            Err(e) => {
                let action = classify_action(&e, default_cooldown_rate_limited);
                let outcome = outcome_code(&e).to_string();
                match action {
                    RetryAction::CooldownAndFailover { cooldown } => {
                        let reason = match &e {
                            LlmError::RateLimited { .. } => {
                                CooldownReason::RateLimited { retry_after_ms: 0 }
                            }
                            LlmError::Overloaded { .. } => CooldownReason::Overloaded,
                            LlmError::Provider { status, .. } => {
                                CooldownReason::Provider5xx { status: *status }
                            }
                            LlmError::Auth => CooldownReason::Auth,
                            _ => CooldownReason::Auth,
                        };
                        ctx.registry
                            .mark_cooldown(&provider_name, &label, cooldown, reason);
                        let cooldown_until_ms = Some(cooldown.as_millis() as u64);
                        let _ = ctx
                            .event_tx
                            .send(Event::new(
                                ctx.sub_id.clone(),
                                EventMsg::Routing(RoutingEvent {
                                    kind: RoutingEventKind::CooldownStarted,
                                    role: "main".into(),
                                    from_credential: Some(label.clone()),
                                    to_credential: None,
                                    reason: outcome.clone(),
                                    cooldown_until_ms,
                                }),
                            ))
                            .await;
                    }
                    RetryAction::RetrySame { delay_ms } => {
                        let ca = per_cred_attempts.entry(cred_ptr).or_insert(0);
                        if *ca >= 2 {
                            // 同一凭证的 RetrySame 已重试上限:强制 failover。
                            // 下方 line 289 会无条件把该 client push 进 `exclude`,
                            // 故 `next_for` 会选另一个凭证。但此前缺了退避
                            // sleep —— 在持久瞬时错误(SseParse / Http)下会以
                            // 无延迟空转 `max_attempts` 轮,顺带刷爆事件流。
                            // 这里补上与「未到上限」分支同样的 `delay_ms` 退避。
                            sleep(Duration::from_millis(delay_ms)).await;
                            let _ = ctx
                                .event_tx
                                .send(Event::new(
                                    ctx.sub_id.clone(),
                                    EventMsg::Routing(RoutingEvent {
                                        kind: RoutingEventKind::Switched,
                                        role: "main".into(),
                                        from_credential: Some(label.clone()),
                                        to_credential: None,
                                        reason: "retry_same_exhausted".into(),
                                        cooldown_until_ms: None,
                                    }),
                                ))
                                .await;
                        } else {
                            sleep(Duration::from_millis(delay_ms)).await;
                        }
                    }
                    RetryAction::GiveUp => {
                        tried.push(TriedCredential {
                            label: label.clone(),
                            outcome: outcome.clone(),
                        });
                        let _ = ctx
                            .event_tx
                            .send(Event::new(
                                ctx.sub_id.clone(),
                                EventMsg::Error(ErrorEvent {
                                    code: error_code(&e).into(),
                                    message: e.to_string(),
                                    details: Some(serde_json::json!({
                                        "provider": provider_name,
                                        "credential_label": label,
                                        "tried": tried.clone(),
                                    })),
                                }),
                            ))
                            .await;
                        return None;
                    }
                    RetryAction::Failover => {}
                }
                tried.push(TriedCredential {
                    label: label.clone(),
                    outcome: outcome.clone(),
                });
                let _ = ctx
                    .event_tx
                    .send(Event::new(
                        ctx.sub_id.clone(),
                        EventMsg::StreamError(StreamErrorEvent {
                            code: error_code(&e).into(),
                            message: e.to_string(),
                            retry_in_ms: match action {
                                RetryAction::CooldownAndFailover { cooldown } => {
                                    cooldown.as_millis() as u64
                                }
                                RetryAction::RetrySame { delay_ms } => delay_ms,
                                _ => 0,
                            },
                            provider: Some(provider_name.clone()),
                            credential_label: Some(label.clone()),
                            tried: Some(tried.clone()),
                        }),
                    ))
                    .await;
                exclude.push(client.clone());
                if !matches!(action, RetryAction::RetrySame { .. }) {
                    let _ = ctx
                        .event_tx
                        .send(Event::new(
                            ctx.sub_id.clone(),
                            EventMsg::Routing(RoutingEvent {
                                kind: RoutingEventKind::Switched,
                                role: "main".into(),
                                from_credential: Some(label.clone()),
                                to_credential: None,
                                reason: outcome,
                                cooldown_until_ms: match action {
                                    RetryAction::CooldownAndFailover { cooldown } => {
                                        Some(cooldown.as_millis() as u64)
                                    }
                                    _ => None,
                                },
                            }),
                        ))
                        .await;
                }
                continue;
            }
        };

        match process_stream_events(
            &mut *s,
            &mut tried,
            &provider_name,
            &label,
            ctx,
            default_cooldown_rate_limited,
        )
        .await
        {
            Ok((tb, tc, usg, prov, lbl, trunc, think)) => {
                break (tb, tc, usg, prov, lbl, trunc, think);
            }
            Err(action) => match action {
                RetryAction::RetrySame { delay_ms } => {
                    // v1.5 review:同凭证 RetrySame 上限 —— 与 stream 初始化
                    // 错误分支的意图对齐:同一凭证最多尝试 2 次,超限强制
                    // failover(exclude 后 next_for 换池中下一个凭证)。
                    // 此前 mid-stream 错误路径没有上限检查,持久 SSE 解析 /
                    // 网络错误会把 max_attempts 全部烧在一个死凭证上,从不
                    // 尝试其他凭证。
                    let ca = per_cred_attempts.entry(cred_ptr).or_insert(0);
                    if *ca >= 2 {
                        let _ = ctx
                            .event_tx
                            .send(Event::new(
                                ctx.sub_id.clone(),
                                EventMsg::Routing(RoutingEvent {
                                    kind: RoutingEventKind::Switched,
                                    role: "main".into(),
                                    from_credential: Some(label.clone()),
                                    to_credential: None,
                                    reason: "retry_same_exhausted".into(),
                                    cooldown_until_ms: None,
                                }),
                            ))
                            .await;
                        exclude.push(client.clone());
                        continue;
                    }
                    sleep(Duration::from_millis(delay_ms)).await;
                    continue;
                }
                RetryAction::Failover => {
                    exclude.push(client.clone());
                    continue;
                }
                RetryAction::CooldownAndFailover { .. } => continue,
                RetryAction::GiveUp => return None,
            },
        }
    };

    // 构造 latest_content:text + tool_use 块。
    state.latest_content.clear();
    // 完整 assistant 文本快照:此时 text_buf 尚未被 move 进 latest_content,
    // 先 clone 一份供 telemetry 落库用(下方 push 会消费 text_buf 所有权)。
    let text_for_telemetry = text_buf.clone();
    if !text_buf.is_empty() {
        state
            .latest_content
            .push(ContentBlock::Text { text: text_buf });
    }
    for (id, name, args) in &tool_calls {
        state.latest_content.push(ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            args: args.clone(),
        });
    }
    // 注意:此前 `cached_tokens` 误累加了 `cache_write_tokens`(复制粘贴 bug),
    // 与下方 `session_usage` 正确使用的 `usage.cached_tokens` 分叉 —— 导致
    // `TurnComplete.usage`(走 total_usage)与预算统计(session_usage)的
    // 缓存读 token 数长期不一致。这里对齐为 `cached_tokens`,并把全部字段
    // 改为 `saturating_add`(与 session_usage 一致),避免长会话累加溢出回绕。
    state.total_usage = TokenUsage {
        input_tokens: state
            .total_usage
            .input_tokens
            .saturating_add(usage.input_tokens),
        output_tokens: state
            .total_usage
            .output_tokens
            .saturating_add(usage.output_tokens),
        cached_tokens: state
            .total_usage
            .cached_tokens
            .saturating_add(usage.cached_tokens),
        cache_write_tokens: state
            .total_usage
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens),
        total_tokens: state
            .total_usage
            .total_tokens
            .saturating_add(usage.total_tokens),
    };
    // M8:记录本次调用的输入 token 数,供下一次 pre_loop 的 compaction
    // 触发判定使用(权威上下文大小,区别于 turn 级累计的 total_usage)。
    state.last_llm_input_tokens = Some(usage.input_tokens);
    {
        let mut su = ctx.session_usage.write();
        su.input_tokens = su.input_tokens.saturating_add(usage.input_tokens);
        su.output_tokens = su.output_tokens.saturating_add(usage.output_tokens);
        su.cached_tokens = su.cached_tokens.saturating_add(usage.cached_tokens);
        su.cache_write_tokens = su
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        su.total_tokens = su.total_tokens.saturating_add(usage.total_tokens);
    }
    if let Some(tracker) = &ctx.quota_tracker {
        tracker.record_usage(&success_provider, &success_label, usage.total_tokens as u64);
        let api_snapshot = tracker
            .check_via_api(&success_provider, &success_label)
            .await;
        let exhausted = match &api_snapshot {
            Some(s) => s.is_exhausted(),
            None => tracker.is_exhausted(&success_provider, &success_label),
        };
        if exhausted {
            let remaining = match &api_snapshot {
                Some(s) if s.resets_at.is_some() => {
                    let now = chrono::Utc::now();
                    let until = s.resets_at.unwrap();
                    std::time::Duration::from_secs((until - now).num_seconds().max(1) as u64)
                }
                _ => tracker.remaining_window(&success_provider, &success_label),
            };
            let window_ends_secs = remaining.as_secs();
            ctx.registry.mark_cooldown(
                &success_provider,
                &success_label,
                remaining,
                reflect_llm::CooldownReason::QuotaExhausted { window_ends_secs },
            );
            let used = tracker
                .used_tokens(&success_provider, &success_label)
                .unwrap_or(0);
            tracing::info!(
                provider = %success_provider,
                label = %success_label,
                used, window_ends_secs,
                via_api = api_snapshot.is_some(),
                "token plan quota exhausted; switching to next credential"
            );
            let _ = ctx
                .event_tx
                .send(reflect_protocol::Event::new(
                    reflect_protocol::EVENT_ID_NONE,
                    reflect_protocol::EventMsg::QuotaExhausted(
                        reflect_protocol::QuotaExhaustedEvent {
                            provider: success_provider.clone(),
                            label: success_label.clone(),
                            used_tokens: used,
                            max_tokens: 0,
                            window_ends_secs,
                        },
                    ),
                ))
                .await;
        }
    }
    if let Some(limit) = *ctx.token_budget.read() {
        if ctx.session_usage.read().total_tokens as u64 >= limit {
            state.budget_exceeded = true;
            return None;
        }
    }
    let bare_model = model.split_once('/').map(|(_, m)| m).unwrap_or(&model);
    let cost_usd = reflect_llm::providers::price(bare_model, &usage);
    let _ = ctx
        .event_tx
        .send(Event::new(
            ctx.sub_id.clone(),
            EventMsg::TokenCount(TokenCountEvent {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_tokens: usage.cached_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                total_tokens: usage.total_tokens,
                cost_usd,
                provider: Some(success_provider.clone()),
                credential_label: Some(success_label.clone()),
            }),
        ))
        .await;

    // v1.x: 追加持久化 record —— 让 CLI 跨进程可见累计。
    // 复用已计算好的 `usage` 与 `cost_usd`(不重新调 `price()`,避免两次调用
    // 对未在 pricing 表的 model 行为不一致)。best-effort:`record()` 失败仅
    // warn,不 abort 当前 turn —— model_call 不能因落盘失败中断主流程。
    if let Some(recorder) = ctx.recorder.clone() {
        recorder
            .record(RolloutRecord::TokenCount {
                turn_id: ctx.turn_id,
                usage: usage.clone(),
                cost_usd,
                at: chrono::Utc::now(),
            })
            .await
            .unwrap_or_else(|e| tracing::warn!("rollout: TokenCount record failed: {e}"));
    }
    if let Some(sink) = ctx.telemetry.as_ref()
        && sink.enabled()
    {
        let model_ref = reflect_telemetry::ModelRef {
            model_id: bare_model.to_string(),
            provider_id: Some(success_provider.clone()),
            role: Some("main".into()),
            source: Some("main_turn".into()),
        };
        // 完整记录本次调用的请求与响应,便于离线分析与 /traces 详情展示。
        // 体积由 `serialize_redacted` 的 16 KiB 字段截断 + 密钥脱敏兜底,
        // 文件按 session 隔离 + 256 KiB×3 轮转,无膨胀失控风险。
        let resp_record = serde_json::json!({
            "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool-calls" },
            "text": text_for_telemetry,
            "tool_calls": tool_calls.iter().map(|(id, name, args)| serde_json::json!({
                "id": id,
                "name": name,
                "arguments": args,
            })).collect::<Vec<_>>(),
        });
        // req_record:完整 messages + system + thinking + cache_control + tool 名;
        // 工具只留名避免存完整 ToolSpec 体量过大。
        let req_record = serde_json::json!({
            "model": model,
            "attempt": attempt,
            "provider": success_provider,
            "credential_label": success_label,
            "messages": serde_json::to_value(&request.messages).unwrap_or(serde_json::Value::Null),
            "system": serde_json::to_value(&request.system).unwrap_or(serde_json::Value::Null),
            "tool_names": request.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
            "thinking": serde_json::to_value(&request.thinking).unwrap_or(serde_json::Value::Null),
            "cache_control": serde_json::to_value(&request.cache_control).unwrap_or(serde_json::Value::Null),
        });
        sink.record_model_call(
            None,
            Some(&ctx.turn_id.to_string()),
            model_ref,
            req_record,
            resp_record,
            reflect_telemetry::UsageSnapshot {
                input_tokens: usage.input_tokens as u64,
                output_tokens: usage.output_tokens as u64,
                cached_tokens: usage.cached_tokens as u64,
                cache_write_tokens: usage.cache_write_tokens as u64,
                total_tokens: usage.total_tokens as u64,
                cost_usd,
            },
            llm_call_started.elapsed().as_millis() as u64,
            attempt,
            reflect_telemetry::SpanStatus::Completed,
            "main_turn",
        );
    }

    // M2-fix: 把本轮 assistant 输出(text + tool_use)提交到会话历史
    {
        let assistant_text: Option<String> = state
            .latest_content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .filter(|s| !s.is_empty());
        let assistant_tool_calls: Vec<reflect_llm::ToolCallRequest> = state
            .latest_content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, args } => Some(reflect_llm::ToolCallRequest {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                }),
                _ => None,
            })
            .collect();
        if assistant_text.is_some() || !assistant_tool_calls.is_empty() {
            state
                .messages
                .messages
                .push(ChatMessage::Assistant(AssistantContent {
                    text: assistant_text,
                    tool_calls: assistant_tool_calls,
                    // 持久化本轮 thinking,使其在多轮上下文中随 assistant 消息往返。
                    // Anthropic provider 序列化时会发回 `{"type":"thinking"}` 块;
                    // OpenAI/Responses provider 忽略该字段(安全)。
                    thinking: (!thinking_buf.is_empty()).then_some(thinking_buf),
                }));
        }
    }

    if state.force_final_answer {
        return Some(GraphNode::CheckStop);
    }

    if tool_calls.is_empty() {
        if truncated && state.auto_continue_count < MAX_AUTO_CONTINUATIONS {
            state.auto_continue_count = state.auto_continue_count.saturating_add(1);
            tracing::info!(
                iteration = state.iteration,
                auto_continue = state.auto_continue_count,
                "output truncated by max_tokens; auto-continuing",
            );
            state
                .messages
                .messages
                .push(ChatMessage::User(reflect_llm::UserContent {
                    blocks: vec![reflect_llm::ContentBlock::Text {
                        text: format!(
                            "<system-reminder>\nYour previous response was cut off \
                               because it reached the output length limit. Continue and complete \
                               your answer. Pick up exactly where your previous message left \
                               off — do not repeat what you already wrote. When you have enough \
                               to answer, finish with: {}.\n</system-reminder>",
                            reflect_prompt::FINAL_ANSWER_TEMPLATE
                        ),
                    }],
                }));
            return Some(GraphNode::PreLoop);
        }
        Some(GraphNode::CheckStop)
    } else {
        Some(GraphNode::ToolExec)
    }
}

/// v1.0 多 Provider 路由:`model_call` 全部 candidate 失败时的收尾路径。
async fn failed(tried: Vec<TriedCredential>, ctx: &NodeContext, code: &'static str) {
    let _ = ctx
        .event_tx
        .send(Event::new(
            ctx.sub_id.clone(),
            EventMsg::Error(ErrorEvent {
                code: code.into(),
                message: format!("all credentials exhausted ({} tried)", tried.len()),
                details: Some(serde_json::json!({ "tried": tried })),
            }),
        ))
        .await;
}
