//! `pre_loop` 节点 — M4:完整的 turn 前置流水线。

use reflect_llm::ChatMessage;
use reflect_protocol::{
    AbortReason, ContextCompactedStrategy, Event, EventMsg, PermissionMode, RolloutRecord,
};
use reflect_recovery::{MetaKind, RecoveryEntry};

use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;

/// `pre_loop` — M4:完整的 turn 前置流水线。
///
/// 当 `ctx.m4` 为 `Some`(生产):
/// 1. 取 `ctx.messages` 中的用户输入,载入 `state.messages`。
/// 2. 对消息列表调 `compactor.compact`;若某策略触发,发出
///    `ContextCompacted` 并更新 `state.compact_triggered`。
/// 3. 按 `active_agent_def.memory` 加载内存(project + user + session 作用域);
///    截断到 8KB。
/// 4. 构建分层 system prompt(core = agent def + memory;
///    ephemeral = tools + skills catalog + iteration reminder)。
/// 5. 计算 `state.effective_tools`(always_on ∪ active_skills)。
/// 6. 把结果存回 `state.messages`,供 `model_call` 使用。
///
/// 当 `ctx.m4` 为 `None`(测试 / 旧路径):直接透传。`state.messages.messages`
/// 保持空;`model_call` 回退到 `ctx.messages`(向后兼容)。
pub async fn pre_loop(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    let Some(m4) = ctx.m4.as_ref() else {
        return Some(GraphNode::ModelCall);
    };

    // Review 2026-06-29 BUG-2 修复: `compact_triggered` 必须先重置再决策,
    // 否则 `AgentState` 跨 turn 持久化时,上一轮触发的 true 会泄漏到本轮,
    // 导致 active file meta-message 反复注入。
    state.compact_triggered = false;

    // 1. 从初始用户输入(`ctx.messages`)播种 `state.messages`。
    // 仅在本 turn 首次进入 pre_loop 时播种;之后 model_call / tool_exec 会把
    // assistant 消息与工具结果 commit 进 state.messages,若每轮都 clear 会
    // 丢弃这些记录 —— 模型每轮只看到原始用户问题,于是重复发同样的工具
    // 调用(M2 多步历史丢失 bug)。
    if !state.history_seeded {
        state.messages.messages.clear();
        for m in &ctx.messages {
            state.messages.messages.push(m.clone());
        }
        state.history_seeded = true;
    }

    // 2. Compact。M8:把**最近一次** LLM 调用上报的 `input_tokens` 透传,
    // 使之优先于本地 `estimate_messages` 启发式(见
    // `Compactor::compact_with_prior_and_tokens`)。
    //
    // 注意:必须用 `last_llm_input_tokens`(最近一次调用的输入,即权威
    // 上下文大小),**不能**用 `total_usage.input_tokens`(turn 内逐次
    // model_call 的累计值)—— 累计值随迭代数线性增长,长 turn 下会在
    // 每次迭代都误触发 compaction(默认阈值 10k,3 次调用 × ~5k 输入
    // 即越线),导致 microcompact / smart_prune / LLM 摘要反复空转。
    // turn 首次 pre_loop 时尚无先验调用(`None`),仅以本地估算为信号。
    let llm_reported = state.last_llm_input_tokens.filter(|n| *n > 0);
    // v1.2 P1-12(已有-B):`/compact` 手动触发 —— 读 + 清零
    // `force_compact_next`,若为 true 则把 `llm_reported` 强制成 `u32::MAX`
    // 让 `before_tokens` 超过任何阈值,compactor 必然运行(microcompact /
    // smart_prune)。这样 `Op::Compact` 设置的标志在本轮 pre_loop 生效。
    let force_compact = {
        let mut g = ctx.force_compact_next.write();
        let v = *g;
        *g = false;
        v
    };
    let llm_reported = if force_compact {
        Some(u32::MAX)
    } else {
        llm_reported
    };
    let (compacted, evt) = m4
        .compactor
        .compact_with_prior_and_tokens(
            state.messages.messages.clone(),
            state.compaction_summary.as_deref(),
            llm_reported,
        )
        .await;
    state.compact_triggered = !matches!(evt.strategy, ContextCompactedStrategy::Noop);
    if state.compact_triggered {
        // 在 `evt` 被移入 wire event 之前捕获我们需要的字段。
        let strategy = evt.strategy;
        let removed_count = evt.removed_messages;

        // 从压缩后的消息列表里抽取 LLM 生成的摘要
        // (它是带 `<summary>...</summary>` 的 System 消息)。
        let summary_text = compacted.iter().find_map(|m| match m {
            ChatMessage::System(s) if s.contains("<summary>") => Some(s.clone()),
            _ => None,
        });
        if summary_text.is_some() {
            state.compaction_summary = summary_text.clone();
        }

        let _ = ctx
            .event_tx
            .send(Event::new(
                ctx.sub_id.clone(),
                EventMsg::ContextCompacted(evt),
            ))
            .await;

        // M5:持久化 Compaction 记录,让 `resume` 能回放。
        //
        // v1.2 P2:空 compaction 守卫。SmartPrune / Microcompact 在消息很少时
        // 可能判定「触发压缩」但 removed_count=0 且无 summary —— 此前每个 turn
        // 都会写一条 `removed_count:0 summary:""` 的噪声(磁盘实证:236 条空
        // compaction vs 75 条 message)。只有真正移除了消息或产生了摘要才落盘。
        if removed_count > 0 || summary_text.as_ref().is_some_and(|s| !s.is_empty()) {
            if let Some(rec) = ctx.recorder.as_ref() {
                let strategy_str = match strategy {
                    ContextCompactedStrategy::Noop => "noop",
                    ContextCompactedStrategy::Microcompact => "microcompact",
                    ContextCompactedStrategy::SmartPrune => "smart_prune",
                    ContextCompactedStrategy::LlMSummarize => "llm_summarize",
                };
                let _ = rec
                    .record(RolloutRecord::Compaction {
                        turn_id: ctx.turn_id,
                        strategy: strategy_str.into(),
                        removed_count,
                        summary: summary_text.unwrap_or_default(),
                    })
                    .await;
            }
        }
    }
    state.messages.messages = compacted;

    // 3. 按 active_agent_def.memory 作用域加载内存。
    let memory_text = match m4
        .memory
        .load_combined(&m4.active_agent_def.memory, &m4.active_agent_def.name)
    {
        Ok(s) => reflect_memory::truncate_for_injection(&s),
        Err(e) => {
            tracing::warn!(?e, "failed to load memory; using empty");
            String::new()
        }
    };

    // 4. 构建分层 system prompt。
    // v1.x Plan mode:active-mode 状态告知。在 `compose_core` 之后追加
    // `## Active Mode` 段(仅在 `PermissionMode::Plan` 下),让 LLM 知道
    // 自己正处于 Plan mode,而不是只在 prompt 里记得「Plan mode 是什么」。
    // 这是修复「plan mode 下调研后不出 plan」的关键 —— 没这段时 LLM
    // 只知道 Plan mode 规则,不知道自己当前是不是已经在 Plan mode 里。
    let mut core = reflect_prompt::LayeredPrompt::compose_core(
        &m4.active_agent_def.system_prompt,
        &memory_text,
    );
    let current_mode = ctx.cfg.permission_mode();
    reflect_prompt::append_active_mode_section(&mut core, current_mode);
    let ephemeral_skills = m4.skills.render_for_system_prompt();
    let reminder = format!("Iteration {}/{}", state.iteration, ctx.max_iterations);
    let mut effective = m4.skills.active_tool_names();
    // v1.x:外部工具(MCP / LSP / plugin / serve 远程)注册即对 LLM 可见 ——
    // 用户显式接入的工具不应被 skills catalog 的 always_on 白名单挡住
    // (GUI 的 MCP/LSP 全部走 Runtime/Mcp 源;CLI 的 MCP 走 Mcp 源,同受益)。
    // 只并集不删减:Builtin 集仍由 catalog 策略(curated)管理。
    effective.extend(ctx.tools_queue.registry().external_tool_names());

    // v1.x Plan mode：从 LLM 可见工具集中移除通用 `write` / `edit`，强制 LLM
    // 使用 `PlanWrite`（`required_permission = Auto`，审批层跳过）。否则 LLM
    // 在「用 write」与「用 PlanWrite」的矛盾 prompt 下倾向选 write，而 write 的
    // `required_permission = Prompt` 会让审批层独立弹窗 —— 即使 `PlanModeGate`
    // hook 放行了 `.reflect/plan/` 路径也救不了，因为审批层只看
    // `required_permission`、不看 hook 决策。Plan mode 是只读规划阶段，本就
    // 不应允许 write/edit 任意路径；`PlanWrite` 内置 `.reflect/plan/` 路径
    // 强制，语义更窄、更安全。`PlanModeGate` hook 的 `.reflect/plan/` 特例
    // 保留作防御性兜底（万一未来有 skill 把 write 显式加回 always_on）。
    if matches!(current_mode, PermissionMode::Plan) {
        effective.remove("write");
        effective.remove("edit");
    }

    // v1.x 功能 6:AgentDefinition 工具硬性过滤(配置了才生效,否则全工具)。
    // 在 skills 过滤之后叠加,语义:白名单 ∩、黑名单 −、readonly 排除变更类。
    // `tools` / `disallowed_tools` / `readonly` 全空 → 不过滤(向后兼容)。
    let def = &m4.active_agent_def;
    if !def.tools.is_empty() {
        effective.retain(|t| def.tools.iter().any(|x| x == t));
    }
    if !def.disallowed_tools.is_empty() {
        effective.retain(|t| !def.disallowed_tools.iter().any(|x| x == t));
    }
    if def.readonly {
        effective.retain(|t| !reflect_tools::READONLY_DENYLIST.contains(&t.as_str()));
    }
    let tool_specs: Vec<reflect_llm::ToolSpec> = ctx
        .tools_queue
        .registry()
        .list_specs()
        .into_iter()
        .filter_map(|s| match s {
            reflect_tools::ToolSpec::Function {
                name,
                description,
                parameters,
                ..
            } => {
                if effective.contains(name.as_str()) {
                    Some(reflect_llm::ToolSpec::Function {
                        name: name.clone(),
                        description: description.clone(),
                        parameters: parameters.clone(),
                    })
                } else {
                    None
                }
            }
        })
        .collect();
    let mut ephemeral = reflect_prompt::LayeredPrompt::compose_ephemeral_with_mode(
        &tool_specs,
        &ephemeral_skills,
        &reminder,
        current_mode,
    );
    // v1.x Plan mode:previous-turn hint。`take_last_abort_reason()` 一次性
    // 消费最近一次中断原因(用户 Esc / 自然 turn 失败);若消费到
    // `UserInterrupt` 且当前 mode 是 Plan,则在 ephemeral block 里追加
    // `## Previous Turn` 段,提示 LLM 「上轮被中断,在 Plan mode 下应该
    // 收尾并调 ExitPlanMode」。take-once 保证只对紧邻的下一个 turn 注
    // 入一次,不会持续唠叨。
    if let Some(AbortReason::UserInterrupt) = ctx.cfg.take_last_abort_reason()
        && matches!(current_mode, PermissionMode::Plan)
    {
        // 插在 `</system-reminder>` 闭合标签之前 —— `compose_ephemeral`
        // 末尾固定是 `</system-reminder>` 字符串(见 builder.rs:201)。
        if let Some(close_tag_pos) = ephemeral.find("</system-reminder>") {
            let mut new_ephemeral = String::with_capacity(ephemeral.len() + 128);
            new_ephemeral.push_str(&ephemeral[..close_tag_pos]);
            new_ephemeral.push_str(
                "\n## Previous Turn\n\
                 Your previous turn was interrupted by the user (Esc/Stop).\n\
                 In Plan mode, this usually means the user wants you to wrap up:\n\
                 call `ExitPlanMode` with your findings as a Markdown plan.\n",
            );
            new_ephemeral.push_str(&ephemeral[close_tag_pos..]);
            ephemeral = new_ephemeral;
        }
    }

    // v1.x Plan mode: 仅注入 plan 文件目录这一运行时信息。PlanWrite → ExitPlanMode
    // 的收尾指引已由 `compose_ephemeral_with_mode` 在 plan mode 下写入「## Important」
    // 段,这里不再重复,避免两处措辞漂移。
    // `PlanWrite` 在 Plan mode 白名单内且 `required_permission = Auto`,
    // 故写盘免审批(此前用通用 `write` 每次都弹「⚠ Approved: write」)。
    if matches!(current_mode, PermissionMode::Plan) {
        let plan_dir = ctx.cfg.current_workspace().join(".reflect/plan");
        if let Some(close_tag_pos) = ephemeral.find("</system-reminder>") {
            let mut new_ephemeral = String::with_capacity(ephemeral.len() + 128);
            new_ephemeral.push_str(&ephemeral[..close_tag_pos]);
            new_ephemeral.push_str(&format!(
                "\n## Plan Mode Active\n\
                 Your plan file directory: {}\n\
                 Pass a `.md` filename as `path` to `PlanWrite`; it lands in this directory automatically.\n",
                plan_dir.display(),
            ));
            new_ephemeral.push_str(&ephemeral[close_tag_pos..]);
            ephemeral = new_ephemeral;
        }
    }

    // 把 core 前置为 system block(比 System message 更清爽
    // —— Anthropic 等 provider 偏好 `system` 数组以支持缓存)。
    // v1.x fix: ephemeral(工具目录 + 答案格式 + 迭代计数)也作为 system block
    // 追加,而非 trailing User 消息。此前作为 User 消息注入时,MiniMax-M3 等
    // 模型在多轮工具调用后会把这条 trailing system-reminder 误认为「最新的
    // 用户输入」,从而遗忘原始问题、回答 "No question provided"。放进 system
    // 数组后,它属于指令上下文,不会与用户问题混淆。
    let mut blocks = vec![reflect_llm::SystemBlock {
        text: core,
        cache_control: None,
        ephemeral: false,
    }];
    if !ephemeral.trim().is_empty() {
        blocks.push(reflect_llm::SystemBlock {
            text: ephemeral,
            cache_control: None,
            ephemeral: true,
        });
    }
    state.system_blocks = reflect_llm::SystemBlocks(blocks);

    // Ephemeral reminder 不再作为 User 消息注入(见上注释),留空以跳过
    // model_call 里的 trailing User 追加。
    state.ephemeral_text = String::new();

    // 5. 保存 effective tools 供 model_call 使用。
    state.effective_tools = tool_specs;

    // 6. v1.1.0 Phase 6 P0:收集上下文恢复元消息。
    //
    // 每轮从 `M4Deps` 的共享源重新计算(Notes 走 JSONL 落盘,
    // SubagentRegistry / FileRecovery 走 in-memory Arc),所以本字段
    // 不需跨 turn 持久化,`AgentState::default()` 已经给空 Vec。
    // 渲染在 `model_call` 完成,见下文 ephemeral push 之后的循环。
    //
    // Phase A 启用:SessionMemory。
    // Phase B 启用:ActiveFiles(post-compact)。
    // Phase C 启用:SubagentRegistry(每轮)。
    if let Some(text) = m4.note_store.as_meta_message() {
        state
            .recovery_meta
            .push(RecoveryEntry::new(MetaKind::SessionMemory, text));
    }
    // 6a. Active File Recovery:仅在 compact 触发后注入,文件读取静默跳过失败项。
    // Review 2026-06-29 BUG-5: 使用 `recover_with_deleted` 把 NotFound
    // 收集到 deleted, 在 meta 顶部提示 LLM。
    if state.compact_triggered {
        let (recovered, deleted) = m4
            .file_recovery
            .recover_with_deleted(&state.messages.messages);
        if !recovered.is_empty() || !deleted.is_empty() {
            let content = m4
                .file_recovery
                .to_meta_message_with_deleted(&recovered, &deleted);
            state
                .recovery_meta
                .push(RecoveryEntry::new(MetaKind::ActiveFiles, content));
        }
    }
    // 6b. Subagent Registry:每轮渲染(防止 LLM 重复 spawn)。
    if let Some(text) = m4.subagent_registry.as_meta_message() {
        state
            .recovery_meta
            .push(RecoveryEntry::new(MetaKind::SubagentRegistry, text));
    }

    Some(GraphNode::ModelCall)
}
