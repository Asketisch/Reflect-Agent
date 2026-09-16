//! `tool_exec` 节点 — 经 queue 分发 tool_use blocks。

use std::sync::Arc;

use tokio::sync::mpsc;

use reflect_llm::ChatMessage;
use reflect_protocol::{
    ContentBlock, Event, EventMsg, PermissionMode, PlanDraftUpdatedEvent, PlanRejectedEvent,
    RolloutRecord, RolloutRecorder, ToolCallEndEvent,
};

use super::nudge::{
    REPEAT_LOOP_THRESHOLD, WEB_HISTORY_CAP, canonical_call_signature, inject_loop_nudge,
    maybe_inject_progress_nudge, url_host,
};
use crate::graph::GraphNode;
use crate::graph::state::{AgentState, WebFetchEntry};
use crate::submission_loop::{
    FALLBACK_PLAN_MARKDOWN, NodeContext, dispatch_plan_ready_blocking,
    dispatch_plan_request_blocking,
};

/// `tool_exec` — 经 queue 分发 tool_use blocks 并把结果追加进
/// `state.latest_content`。为每个工具发出 `ToolCallEnd`。
#[tracing::instrument(
    name = "agent.tool_exec",
    level = "info",
    skip_all,
    fields(sub_id = %ctx.sub_id)
)]
pub async fn tool_exec(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    let calls: Vec<_> = state
        .latest_content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, args } => Some(reflect_tools::ToolCallRequest {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Some(GraphNode::CheckStop);
    }
    // v1.x loop-guard:重复调用检测。把本轮所有调用的 (name, args) 规范化为
    // 一个稳定签名,与上一轮比较。连续命中阈值时不再执行重复的工具,而是
    // 把本轮 ToolUse 从 latest_content 移除、提交一条 system-reminder 提示
    // 模型停止重复并基于已有结果作答,然后回到 model_call(无工具调用 →
    // 自然走 CheckStop 产出最终答案)。这是历史持久化修复之上的额外防御。
    let sig = canonical_call_signature(&calls);
    let hit = match &state.last_call_signature {
        Some(prev) if prev == &sig => state.repeat_hit_count.saturating_add(1),
        _ => 1,
    };
    state.last_call_signature = Some(sig);
    state.repeat_hit_count = hit;
    if hit >= REPEAT_LOOP_THRESHOLD {
        tracing::warn!(
            hit,
            threshold = REPEAT_LOOP_THRESHOLD,
            "loop-guard: detected repeated identical tool calls; nudging model to answer"
        );
        // 移除本轮 ToolUse 块(未执行),保留可能存在的文本。
        state
            .latest_content
            .retain(|b| !matches!(b, ContentBlock::ToolUse { .. }));
        // 关键:上一轮 model_call 已经把带 tool_calls 的 assistant 消息写入
        // `state.messages.messages`(model_call/mod.rs:498-530)。Anthropic /
        // OpenAI 要求历史里每个 tool_use id 都必须有配对的 tool_result,否则
        // 下一轮请求会 400("tool_use ids found without tool_result")。
        // 这里不执行工具,但必须为每个被抑制的 call_id 补一条
        // `ToolResult(is_error=true)`,告知模型「该调用被循环守卫抑制」,
        // 否则孤立 tool_use 会把整个 turn 打死(400 → GiveUp)。
        for c in &calls {
            state
                .messages
                .messages
                .push(ChatMessage::Tool(reflect_llm::ToolResult {
                    call_id: c.id.clone(),
                    content: vec![reflect_llm::ContentBlock::Text {
                        text: "This tool call was suppressed by the loop-guard \
                               (repeated identical calls detected). Stop calling \
                               tools and answer using the results you already have."
                            .into(),
                    }],
                    is_error: true,
                }));
        }
        // 提交 assistant(若本轮只有 tool calls 无文本,补一个空占位避免
        // 连续两条 assistant;此处直接注入提醒消息即可)。
        inject_loop_nudge(state);
        // 重置计数,避免下一轮立即再触发。
        state.repeat_hit_count = 0;
        state.last_call_signature = None;
        return Some(GraphNode::PreLoop);
    }
    // v1.2 P1:记录 call_id → tool_name + args 映射,供 telemetry 在结果
    // 返回后恢复工具名 / 参数(retain 会移除 ToolUse blocks)。
    let call_meta: std::collections::HashMap<String, (String, serde_json::Value)> = calls
        .iter()
        .map(|c| (c.id.clone(), (c.name.clone(), c.args.clone())))
        .collect();
    // v1.4 C1:子代理自报告 —— 本线程若是父会话 spawn 的子代理
    // (cfg.subagent_status 为 Some),把本批工具开始写入自己的状态槽,
    // 父会话经 QuerySubagents 即时可见。
    if let Some(slot) = ctx.cfg.subagent_status.as_ref() {
        for c in &calls {
            slot.begin_tool(&c.name);
        }
    }
    // v1.4 C1:构造事件转发器 —— 携带 sub_id / event_tx / 父历史尾部
    // 快照,供 CallSubAgentTool 转发子代理进度(SubagentProgress)与
    // 按 pass_context_messages 截取上下文。普通工具不读这些字段,零开销。
    // 快照:最近 10 条消息(含本轮 assistant 工具调用),JSON 形态规避
    // reflect-tools → llm 反向依赖。
    let parent_tail: Vec<serde_json::Value> = state
        .messages
        .messages
        .iter()
        .rev()
        .take(10)
        .rev()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    let mut forwarder = reflect_tools::ToolEventForwarder::new(
        ctx.sub_id.clone(),
        ctx.event_tx.clone(),
        parent_tail,
    );
    // v1.5 R1:每回合沙箱覆盖透传给队列(→ ToolContext.os_sandbox)。
    forwarder.os_sandbox = ctx.sandbox_override;
    let forwarder = Arc::new(forwarder);
    let results = ctx
        .tools_queue
        .execute_all_with_progress(calls, ctx.approval_gate.clone(), Some(forwarder))
        .await;
    // v1.4 C1:本批工具全部结束 → 清空子代理状态槽的 current_tool。
    if let Some(slot) = ctx.cfg.subagent_status.as_ref() {
        for (name, _) in call_meta.values() {
            slot.end_tool(name);
        }
    }
    // 移除 ToolUse 块,用它们的结果替换。
    state
        .latest_content
        .retain(|b| !matches!(b, ContentBlock::ToolUse { .. }));
    for mut r in results {
        // 取出 call_meta 提前 —— v1.x Plan mode 分支要在 ToolCallEnd 之后
        // 立刻 dispatch(成功走 request / ready;失败走 PlanRejected),需要
        // 工具名 + 参数。
        let (tool_name, args) = call_meta
            .get(&r.call_id)
            .cloned()
            .unwrap_or(("unknown".into(), serde_json::Value::Null));
        // 为 wire 协议发出 ToolCallEnd。
        let _ = ctx
            .event_tx
            .send(Event::new(
                ctx.sub_id.clone(),
                EventMsg::ToolCallEnd(ToolCallEndEvent {
                    call_id: r.call_id.clone(),
                    output: reflect_protocol::ToolOutput {
                        content: r.content.clone(),
                        is_error: r.is_error,
                        metadata: r.metadata.clone(),
                        elapsed_ms: r.elapsed_ms,
                    },
                    is_error: r.is_error,
                    elapsed_ms: r.elapsed_ms,
                    child_id: None,
                }),
            ))
            .await;
        // v1.x Plan mode:LLM 调 `EnterPlanModeTool` / `ExitPlanModeTool`
        // 时,工具本身只回 ToolOutput(成功 + metadata);真正的
        // `PlanRequest` / `PlanReady` 事件 + 用户审批 + 模式翻转由
        // core 在 tool_exec 末尾按 tool_name 派发。
        //
        // 关键不变量(回归保护):
        // - 工具调用失败(`is_error`)→ **不**发 `PlanRequest` /
        //   `PlanReady`,改发 `PlanRejected { reason }`。避免「工具
        //   报参数错却触发 Plan 审批 round-trip」的语义错位。
        // - `markdown` 解析顺序:`args["markdown"]` 非空 → 用之;
        //   否则取 `state.latest_content` 中最近一段 assistant text
        //   block(典型场景:agent 先输出「## Plan ...」再调工具);
        //   否则走 [`FALLBACK_PLAN_MARKDOWN`] 确定性安全回退。
        match tool_name.as_str() {
            "EnterPlanMode" => {
                // v1.x 防御性兜底:若用户已通过 Shift+Tab 进入 Plan 模式,
                // LLM 仍调 `EnterPlanMode` 是冗余调用 —— 跳过
                // `PlanRequest` 派发,避免「Enter Plan Mode」弹窗残留
                // (用户已确认过,无需再次进入)。`PlanModeGate` hook
                // 应已 deny 此调用,此处兜底 hook 未注册/被跳过的情况。
                if *ctx.cfg.permission_mode.read() == PermissionMode::Plan {
                    tracing::info!(
                        tool_call_id = %r.call_id,
                        "EnterPlanMode called but already in Plan mode; \
                         skipping PlanRequest dispatch"
                    );
                } else if let (Some(gate), Some(subs)) = (
                    ctx.plan_approval_gate.as_ref(),
                    ctx.plan_session_subs.as_ref(),
                ) {
                    if r.is_error {
                        emit_plan_rejected(
                            ctx.event_tx.clone(),
                            ctx.sub_id.clone(),
                            format!("EnterPlanMode tool failed: {}", summarize_tool_error(&r)),
                            ctx.recorder.clone(),
                        )
                        .await;
                    } else {
                        let task = args
                            .get("task")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if task.is_empty() {
                            // 防御性:工具校验层已拒,但万一有非空 trim
                            // 异常,以「工具调用方出错」语义拒绝。
                            emit_plan_rejected(
                                ctx.event_tx.clone(),
                                ctx.sub_id.clone(),
                                "EnterPlanMode tool returned empty 'task'".to_string(),
                                ctx.recorder.clone(),
                            )
                            .await;
                        } else {
                            let _ = dispatch_plan_request_blocking(
                                gate,
                                subs,
                                ctx.event_tx.clone(),
                                ctx.sub_id.clone(),
                                ctx.cfg.clone(),
                                task,
                            )
                            .await;
                        }
                    }
                }
            }
            "ExitPlanMode" => {
                if let (Some(gate), Some(subs)) = (
                    ctx.plan_approval_gate.as_ref(),
                    ctx.plan_session_subs.as_ref(),
                ) {
                    if r.is_error {
                        emit_plan_rejected(
                            ctx.event_tx.clone(),
                            ctx.sub_id.clone(),
                            format!("ExitPlanMode tool failed: {}", summarize_tool_error(&r)),
                            ctx.recorder.clone(),
                        )
                        .await;
                    } else {
                        // 优先从 plan 文件读取（agent 在 Plan mode 下用 write
                        // 写入 .reflect/plan/）。读到非空内容则用之，否则回退
                        // 到 resolve_plan_markdown 的 args/latest_content 逻辑。
                        let workspace = ctx.cfg.current_workspace();
                        let markdown = read_latest_plan_file(&workspace)
                            .unwrap_or_else(|| resolve_plan_markdown(&args, &state.latest_content));
                        // 阻塞等待用户审批。无论批准/Revise 都回 PreLoop,
                        // 下一轮按已翻转的 mode 自然推进。
                        let choice = dispatch_plan_ready_blocking(
                            gate,
                            subs,
                            ctx.event_tx.clone(),
                            ctx.sub_id.clone(),
                            ctx.cfg.clone(),
                            markdown,
                        )
                        .await;
                        // v1.4 审批信号回传:choice 此前被丢弃,模型收到的
                        // 工具输出始终是 "Plan ready (N chars)",感知不到
                        // 「用户已批准 / 要求修改」——真实 LLM 在 ExitPlanMode
                        // 返回后倾向直接结束 turn("Waiting for your
                        // approval to proceed."),已批准的 plan 永远不会被执行。
                        // 这里把决策重写进工具输出(模型可见的 Tool 消息 +
                        // latest_content),驱动模型在批准后立即执行 plan。
                        // 注:ToolCallEnd 事件(上方)发的是原始输出,TUI
                        // 侧另有 PlanApproved 确认条,不受影响。
                        if let Some(choice) = choice {
                            match choice {
                                reflect_protocol::PlanApprovalChoice::AutoMode
                                | reflect_protocol::PlanApprovalChoice::ManualApprove => {
                                    rewrite_first_text_block(
                                        &mut r,
                                        "Plan approved. The user approved this \
                                         plan and the permission mode has been \
                                         switched accordingly. Execute the plan \
                                         now: perform the planned steps and \
                                         report the result.",
                                    );
                                }
                                reflect_protocol::PlanApprovalChoice::Revise => {
                                    rewrite_first_text_block(
                                        &mut r,
                                        "The user requested changes to the plan. \
                                         Stay in Plan mode: refine the plan \
                                         according to the user's feedback, update \
                                         it with PlanWrite, then call ExitPlanMode \
                                         again to request approval.",
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // v1.x Plan mode 草稿预览:写盘到 `.reflect/plan/` 后即时 emit
        // `PlanDraftUpdated`,让 TUI 把 plan 草稿推到对话流。**不阻塞** agent
        // turn,也**不**触发 approval modal —— 真正的 1/2/3 决策仍走
        // `ExitPlanMode` → `PlanReady`。best-effort:任何 IO / 路径不匹配都
        // 静默退出,不影响主流程。
        if !r.is_error {
            maybe_emit_plan_draft_updated(
                &tool_name,
                &args,
                &ctx.cfg.current_workspace(),
                ctx.event_tx.clone(),
                ctx.sub_id.clone(),
            )
            .await;
        }
        // v1.2 P1:写本地 telemetry —— tool.call.ended span。best-effort。
        if let Some(sink) = ctx.telemetry.as_ref()
            && sink.enabled()
        {
            // 输出预览(取第一个 text block,截断到 2 KiB)。
            let output_preview: String = r
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            // 从执行前记录的 call_meta 恢复工具名 + args。
            let (tool_name, args) = call_meta
                .get(&r.call_id)
                .cloned()
                .unwrap_or(("unknown".into(), serde_json::Value::Null));
            sink.record_tool_call(
                None,
                Some(&ctx.turn_id.to_string()),
                &r.call_id,
                &tool_name,
                &args,
                &output_preview,
                r.is_error,
                r.elapsed_ms,
            );
        }
        // M2-fix: 把工具结果作为 Tool 消息提交进会话历史,使下一轮 model_call
        // 能看到「assistant 调用了 X → X 返回了 Y」。此前只放进 latest_content
        // (图内决策用),模型永远看不到工具输出,导致重复调用同一工具。
        let call_id = r.call_id.clone();
        let is_error = r.is_error;
        let tool_output = reflect_protocol::ToolOutput {
            content: r.content.clone(),
            is_error: r.is_error,
            metadata: r.metadata.clone(),
            elapsed_ms: r.elapsed_ms,
        };
        // v1.x progress-nudge:对 web_fetch 抓取结果,在 Tool 消息之外再写一份
        // 「已收集事实」条目到 state。tool_exec 结束后根据 web_fetch_history
        // + web_domain_counts 决定是否注入结构化摘要(system-reminder),
        // 对抗模型在多源检索中无法收敛的问题。
        let (tool_name_for_history, _args_for_history) = call_meta
            .get(&call_id)
            .cloned()
            .unwrap_or(("unknown".into(), serde_json::Value::Null));
        if tool_name_for_history == "web_fetch" {
            let url = tool_output
                .metadata
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !url.is_empty() {
                let snippet = tool_output
                    .content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let snippet: String = snippet.chars().take(240).collect();
                let bytes = tool_output
                    .metadata
                    .get("bytes_read")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                state.web_fetch_history.push(WebFetchEntry {
                    url,
                    snippet,
                    bytes,
                });
                if state.web_fetch_history.len() > WEB_HISTORY_CAP {
                    let drop = state.web_fetch_history.len() - WEB_HISTORY_CAP;
                    state.web_fetch_history.drain(0..drop);
                }
                if let Some(domain) = url_host(&state.web_fetch_history.last().unwrap().url) {
                    *state.web_domain_counts.entry(domain).or_insert(0) += 1;
                }
            }
        }
        state
            .messages
            .messages
            .push(ChatMessage::Tool(reflect_llm::ToolResult::from_output(
                &call_id,
                &tool_output,
            )));
        // is_error 用于循环检测日志,避免 unused 警告。
        let _ = is_error;
        state.latest_content.push(r.into_content_block());
    }
    // v1.x progress-nudge:本轮所有工具执行完后,根据 web_fetch 次数 / 域名
    // 重复 / 迭代剩余注入结构化「已收集事实」+「请立即作答」提示。这是循环
    // 检测器(同签名)之外的额外机制,处理两类真实失败模式:① 不同 URL 但同
    // 主题重复检索无法收敛;② 多步推理跑满迭代却没产出 `FINAL ANSWER:`
    // (GAIA 截断类失败的主因)。已重新启用,原注释「暂禁用」作废。
    maybe_inject_progress_nudge(state, ctx.max_iterations);
    // 工具执行完毕后回到 PreLoop → ModelCall,让模型基于工具结果继续推理。
    // 此前返回 CheckStop 会让默认 Stop hook(无拒绝)直接终止 turn,
    // 导致 agent 在首次工具调用后即停、无法多步工作(M2 多步回退 bug)。
    // max_iterations 安全阀在 model_call 入口兜底,避免无限循环。
    Some(GraphNode::PreLoop)
}

// ── v1.x Plan mode helpers (LLM-driven path) ──────────────────────────
//
// 与 `submission_loop` 中 `dispatch_plan_request` / `dispatch_plan_ready`
// 配套使用:这三个函数处理 tool_exec 检测到 plan-mode 工具调用后的
// 派生事件路径(成功 → dispatch;失败 → PlanRejected)。

/// v1.x Plan mode:EnterPlanModeTool / ExitPlanModeTool 失败时,发
/// `PlanRejected { plan_id: <synthesized>, reason }` 让 TUI 弹 plan
/// reject 反馈(而不是悄悄走 ToolCallEnd 然后让 agent 重试)。
///
/// 用一个**新的** `PlanId`(而不是 call_id)是因为 plan_id 是与
/// PlanApproval gate oneshot 配对的 stable handle;这里没有挂 waiter
/// (工具已失败,根本不进入审批 round-trip),但保持 plan_id 形式
/// 一致让 TUI reducer 单一分支处理 `PlanRejected`,无须为「
/// 工具失败」单开一类事件。
async fn emit_plan_rejected(
    turn_tx: mpsc::Sender<Event>,
    sub_id: String,
    reason: String,
    recorder: Option<Arc<dyn RolloutRecorder>>,
) {
    let plan_id = reflect_protocol::PlanId::new();
    // v1.x:把 tool 失败合成的 PlanRejected 也落进 session JSONL,确保
    // 同一 chat 内所有 plan 操作都被记录(与 dispatch_plan_ready 配对)。
    if let Some(rec) = recorder {
        let at = chrono::Utc::now();
        let rec_reason = Some(reason.clone());
        tokio::spawn(async move {
            if let Err(e) = rec
                .record(RolloutRecord::PlanRejected {
                    plan_id,
                    reason: rec_reason,
                    at,
                })
                .await
            {
                tracing::warn!(error = %e, "rollout: failed to persist plan_rejected (tool path)");
            }
        });
    }
    let _ = turn_tx
        .send(Event::new(
            sub_id,
            EventMsg::PlanRejected(PlanRejectedEvent {
                plan_id,
                reason: Some(reason),
            }),
        ))
        .await;
}

/// 把 `ToolResult.content` 里第一个 text block 截前 ~200 字符作为
/// 「失败原因」文本。EnterPlanMode / ExitPlanMode 的工具实现把
/// `InvalidArgs` / `Execution` 等错误用 `Text { text: ... }` 形式
/// 塞进 content(沿用 `ToolExecutionQueue::execute_single` 通用约定),
/// 此函数纯文本抽取即可。
/// 重写 `ToolResult.content` 里第一个 text block(把 plan 审批决策
/// 回传给模型用)。已有 text block 则覆盖;没有则追加一个。
fn rewrite_first_text_block(r: &mut reflect_tools::ToolResult, text: &str) {
    if let Some(b) = r
        .content
        .iter_mut()
        .find(|b| matches!(b, ContentBlock::Text { .. }))
    {
        if let ContentBlock::Text { text: t } = b {
            *t = text.to_string();
        }
    } else {
        r.content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
}

fn summarize_tool_error(r: &reflect_tools::ToolResult) -> String {
    r.content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.chars().take(200).collect::<String>())
                }
            }
            _ => None,
        })
        .unwrap_or_else(|| "(no detail)".to_string())
}

/// v1.x Plan mode:`ExitPlanModeTool` 调成功时,markdown 解析顺序:
/// 1. `args["markdown"]` 非空 → 用之(agent 显式给的内容优先)
/// 2. `state.latest_content` 中最近一段 assistant `Text` block → 用之
///    (典型场景:agent 在工具调用前已经发出 `## Plan\n- step1\n- step2`
///    摘要,这里直接拼出来)
/// 3. 兜底 [`FALLBACK_PLAN_MARKDOWN`] —— 确定性安全字符串,而不是
///    会误导用户的「待 Phase 5 接入」placeholder。
fn resolve_plan_markdown(args: &serde_json::Value, latest_content: &[ContentBlock]) -> String {
    if let Some(s) = args.get("markdown").and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // 倒序找最近一段非空 assistant Text block。
    for block in latest_content.iter().rev() {
        if let ContentBlock::Text { text } = block {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    FALLBACK_PLAN_MARKDOWN.to_string()
}

/// 扫描 `<workspace>/.reflect/plan/` 目录，返回最新修改的 `.md` 文件内容。
///
/// agent 在 Plan mode 下用 write 工具把完整 plan markdown 写入 plan 文件，
/// ExitPlanMode 调用时从此处读取而非依赖工具参数。目录不存在或为空时返回
/// `None`（调用方回退到 args/latest_content）。best-effort：IO 错误静默降级。
fn read_latest_plan_file(workspace: &std::path::Path) -> Option<String> {
    let plan_dir = workspace.join(".reflect/plan");
    let latest = std::fs::read_dir(&plan_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)?;
    let content = std::fs::read_to_string(&latest).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// v1.x Plan mode 草稿预览:解析工具调用是否写到了 `<workspace>/.reflect/plan/`
/// 之下,若是则返回 canonicalize 后的绝对路径,否则返回 `None`。
///
/// 三种工具的路径语义:
/// - **PlanWrite** —— `args["path"]` 是文件名/相对子路径,工具内部强制拼到
///   `.reflect/plan/` 下(已通过 canonicalize + strip_prefix 验证),所以
///   这里直接 join,不再重复 strip_prefix 检查。
/// - **write / edit** —— `args["path"]` 是 workspace 相对路径或绝对路径,
///   需要独立 canonicalize 后判断 `starts_with(plan_dir)`,避免把任意路径
///   错误地当成 plan 草稿事件广播。
///
/// `canonicalize` 失败(文件不存在 / 权限)时返回 `None` —— 此函数只在
/// `ToolCallEnd(is_error = false)` 之后调用,文件应当已存在,canonicalize
/// 失败多半是 race 或权限问题,best-effort 静默跳过即可。
fn resolve_plan_draft_path(
    tool_name: &str,
    args: &serde_json::Value,
    workspace: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let plan_dir = workspace.join(".reflect/plan");
    let plan_dir_canon = plan_dir.canonicalize().ok()?;

    let raw = args.get("path").and_then(|v| v.as_str())?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let candidate = match tool_name {
        "PlanWrite" => plan_dir.join(raw),
        "write" | "edit" => {
            let p = std::path::Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace.join(p)
            }
        }
        _ => return None,
    };

    let canon = candidate.canonicalize().ok()?;
    if !canon.starts_with(&plan_dir_canon) {
        return None;
    }
    Some(canon)
}

/// v1.x Plan mode 草稿预览的派发器:若工具调用写到了 plan_dir 之下,
/// 读出文件内容,emit `PlanDraftUpdated` 给所有 session 订阅者(TUI)。
///
/// 设计要点:
/// - **不阻塞** —— 不注册 `PlanApprovalGate` waiter,agent turn 自然推进;
///   用户做 1/2/3 决策仍走 `ExitPlanMode` → `PlanReady` → approval modal。
/// - **多次 emit 即多次刷新** —— 同一文件多次写盘会触发多次事件,TUI
///   端可据 `draft_id` 辨识是否同一草稿的迭代,做覆盖式更新。
/// - **空文件不 emit** —— 避免工具调用失败但 is_error=false 的边界场景
///   把空 plan 推给 TUI。
async fn maybe_emit_plan_draft_updated(
    tool_name: &str,
    args: &serde_json::Value,
    workspace: &std::path::Path,
    event_tx: mpsc::Sender<Event>,
    sub_id: String,
) {
    let path = match resolve_plan_draft_path(tool_name, args, workspace) {
        Some(p) => p,
        None => return,
    };
    let markdown = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "plan_draft_updated: failed to read plan file after tool write"
            );
            return;
        }
    };
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return;
    }

    // draft_id 取文件名(不含扩展名),便于 TUI 标识同一草稿的迭代。
    let draft_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("draft")
        .to_string();

    tracing::info!(
        draft_id = %draft_id,
        path = %path.display(),
        len = trimmed.len(),
        "emitting PlanDraftUpdated for in-flight plan preview"
    );

    let _ = event_tx
        .send(Event::new(
            sub_id,
            EventMsg::PlanDraftUpdated(PlanDraftUpdatedEvent {
                draft_id,
                markdown: trimmed.to_string(),
                path: Some(path),
            }),
        ))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.to_string(),
        }
    }

    // ── resolve_plan_markdown:markdown 解析顺序 ──

    #[test]
    fn resolve_plan_markdown_uses_explicit_args_markdown_when_present() {
        let args = serde_json::json!({"markdown": "## Plan\n- step1"});
        let out = resolve_plan_markdown(&args, &[]);
        assert_eq!(out, "## Plan\n- step1");
    }

    #[test]
    fn resolve_plan_markdown_ignores_whitespace_only_args_markdown() {
        // 空白 markdown 视同未提供,回退到 latest_content。
        let args = serde_json::json!({"markdown": "   \n\t  "});
        let latest = vec![text("## Plan from assistant\n- step2")];
        let out = resolve_plan_markdown(&args, &latest);
        assert_eq!(out, "## Plan from assistant\n- step2");
    }

    #[test]
    fn resolve_plan_markdown_falls_back_to_latest_assistant_text() {
        // 典型场景:agent 先输出 `## Plan ...` 文本块,再调 ExitPlanMode(无 markdown)。
        let args = serde_json::json!({});
        let latest = vec![
            text("thinking..."),
            text("## Plan\n1. read foo.rs\n2. edit bar.rs"),
            ContentBlock::ToolUse {
                id: "x".into(),
                name: "ExitPlanMode".into(),
                args: serde_json::json!({}),
            },
        ];
        // 倒序:应取最近一段非空 assistant Text(跳过 ToolUse / 空块)。
        let out = resolve_plan_markdown(&args, &latest);
        assert!(out.contains("## Plan"));
        assert!(out.contains("read foo.rs"));
    }

    #[test]
    fn resolve_plan_markdown_falls_back_when_latest_content_empty() {
        let args = serde_json::json!({});
        let out = resolve_plan_markdown(&args, &[]);
        assert_eq!(out, FALLBACK_PLAN_MARKDOWN);
        // 兜底必须可被断言为非 placeholder 性质,避免误导用户。
        assert!(!out.contains("placeholder"));
    }

    #[test]
    fn resolve_plan_markdown_skips_blank_assistant_text_blocks() {
        // 中间空 Text 块不应被当成「最近非空」。
        let args = serde_json::json!({});
        let latest = vec![text("   \n"), text(""), text("## Plan\n- real")];
        let out = resolve_plan_markdown(&args, &latest);
        assert_eq!(out, "## Plan\n- real");
    }

    // ── summarize_tool_error ──

    #[test]
    fn summarize_tool_error_extracts_first_text_block() {
        let r = reflect_tools::ToolResult {
            call_id: "c1".into(),
            content: vec![
                text("InvalidArgs: missing 'task'"),
                text("should be ignored"),
            ],
            is_error: true,
            elapsed_ms: 0,
            metadata: serde_json::json!({}),
        };
        let s = summarize_tool_error(&r);
        assert!(s.contains("missing 'task'"));
        assert!(!s.contains("ignored"));
    }

    #[test]
    fn summarize_tool_error_truncates_long_messages() {
        let long = "x".repeat(500);
        let r = reflect_tools::ToolResult {
            call_id: "c1".into(),
            content: vec![text(&long)],
            is_error: true,
            elapsed_ms: 0,
            metadata: serde_json::json!({}),
        };
        let s = summarize_tool_error(&r);
        // 截到 ~200 字符(char-based)。
        assert!(s.chars().count() <= 200);
        assert!(s.chars().count() >= 100);
    }

    #[test]
    fn summarize_tool_error_falls_back_when_no_text() {
        let r = reflect_tools::ToolResult {
            call_id: "c1".into(),
            content: vec![],
            is_error: true,
            elapsed_ms: 0,
            metadata: serde_json::json!({}),
        };
        assert_eq!(summarize_tool_error(&r), "(no detail)");
    }

    #[test]
    fn summarize_tool_error_skips_whitespace_only_text() {
        let r = reflect_tools::ToolResult {
            call_id: "c1".into(),
            content: vec![text("   \n\t ")],
            is_error: true,
            elapsed_ms: 0,
            metadata: serde_json::json!({}),
        };
        assert_eq!(summarize_tool_error(&r), "(no detail)");
    }

    #[test]
    fn fallback_plan_markdown_is_not_placeholder() {
        // 防回归:兜底字符串绝不能是误导性的「待 Phase 5 接入」placeholder。
        // 也不应被任何「final plan」文本污染,避免用户看到空白计划。
        assert!(!FALLBACK_PLAN_MARKDOWN.contains("placeholder"));
        assert!(!FALLBACK_PLAN_MARKDOWN.contains("Phase"));
        assert!(!FALLBACK_PLAN_MARKDOWN.is_empty());
    }

    // ── v1.x Plan mode 草稿预览:resolve_plan_draft_path 单元测试 ────────

    use std::fs;
    use std::path::PathBuf;

    /// 创建临时 workspace + `.reflect/plan/` 目录,返回 (workspace, plan_dir)。
    /// 测试结束自动清理。
    struct TempWorkspace {
        ws: PathBuf,
    }
    impl TempWorkspace {
        fn new() -> Self {
            let ws = std::env::temp_dir().join(format!(
                "reflect-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(ws.join(".reflect/plan")).unwrap();
            Self { ws }
        }
        fn write_plan(&self, name: &str, content: &str) -> PathBuf {
            let p = self.ws.join(".reflect/plan").join(name);
            fs::write(&p, content).unwrap();
            p
        }
    }
    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.ws);
        }
    }

    #[test]
    fn resolve_plan_draft_path_recognizes_planwrite() {
        let tws = TempWorkspace::new();
        tws.write_plan("refactor.md", "## Plan\n- step");
        let args = serde_json::json!({"path": "refactor.md", "content": "## Plan\n- step"});
        let resolved = resolve_plan_draft_path("PlanWrite", &args, &tws.ws);
        assert!(resolved.is_some(), "PlanWrite 应识别为 plan 草稿");
        let p = resolved.unwrap();
        assert!(p.ends_with("refactor.md"));
    }

    #[test]
    fn resolve_plan_draft_path_recognizes_write_into_plan_dir() {
        let tws = TempWorkspace::new();
        tws.write_plan("foo.md", "draft");
        let abs = tws.ws.join(".reflect/plan/foo.md");
        let args = serde_json::json!({"path": abs.to_string_lossy(), "content": "draft"});
        let resolved = resolve_plan_draft_path("write", &args, &tws.ws);
        assert!(resolved.is_some(), "write 到 .reflect/plan/ 应识别");
    }

    #[test]
    fn resolve_plan_draft_path_rejects_write_outside_plan_dir() {
        let tws = TempWorkspace::new();
        // 写到 workspace 根目录(非 plan_dir)。
        let outside = tws.ws.join("README.md");
        fs::write(&outside, "readme").unwrap();
        let args = serde_json::json!({"path": outside.to_string_lossy(), "content": "readme"});
        let resolved = resolve_plan_draft_path("write", &args, &tws.ws);
        assert!(resolved.is_none(), "写到 plan_dir 外不应识别为草稿");
    }

    #[test]
    fn resolve_plan_draft_path_rejects_unknown_tool() {
        let tws = TempWorkspace::new();
        tws.write_plan("x.md", "x");
        let args = serde_json::json!({"path": "x.md"});
        let resolved = resolve_plan_draft_path("read", &args, &tws.ws);
        assert!(resolved.is_none(), "非 write/edit/PlanWrite 不应识别");
    }

    #[test]
    fn resolve_plan_draft_path_rejects_missing_path_arg() {
        let tws = TempWorkspace::new();
        let args = serde_json::json!({"content": "x"});
        let resolved = resolve_plan_draft_path("PlanWrite", &args, &tws.ws);
        assert!(resolved.is_none(), "缺 path 参数应返回 None");
    }

    #[test]
    fn resolve_plan_draft_path_rejects_nonexistent_plan_dir() {
        // workspace 存在但 .reflect/plan/ 不存在 → canonicalize 失败 → None。
        let ws = std::env::temp_dir().join(format!(
            "reflect-test-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&ws).unwrap();
        let args = serde_json::json!({"path": "x.md", "content": "x"});
        let resolved = resolve_plan_draft_path("PlanWrite", &args, &ws);
        assert!(resolved.is_none(), "plan_dir 不存在时应当返回 None");
        let _ = fs::remove_dir_all(&ws);
    }
}
