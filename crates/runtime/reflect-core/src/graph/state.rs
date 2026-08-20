//! `AgentState` — 4 节点 StateGraph 的状态(M2)。
//!
//! 承载每轮的状态(messages、迭代计数、工具调用等)。M1 不使用它,
//! 由 `submission_loop` 隐式持有状态;M2 把状态迁入 `AgentState` 并
//! 沿节点链向下传递。

use reflect_llm::{ChatMessage, SystemBlocks, ToolSpec};
use reflect_protocol::{ContentBlock, TokenUsage};
use reflect_recovery::RecoveryEntry;
use std::collections::HashMap;

/// 完整对话消息历史(system + user + assistant + tool)。
#[derive(Debug, Default)]
pub struct MessageHistory {
    pub messages: Vec<ChatMessage>,
}

/// 每轮可变状态。
#[derive(Debug, Default)]
pub struct AgentState {
    /// 完整对话历史。M4:这是规范存储;`pre_loop` 从 `NodeContext.messages`
    /// 填充,随后 `model_call` 从中读取。
    pub messages: MessageHistory,
    /// 当前迭代计数(自 session 起始的 model_call 计数)。
    pub iteration: u32,
    /// 本 turn 内 Stop hook 否决完成的次数。
    pub stop_hook_attempts: u32,
    /// 搜索调用次数(M3 中供 `search_budget` hook 使用)。
    pub search_calls: u32,
    /// 本 turn 是否已触发 compaction。
    pub compact_triggered: bool,
    /// 本 turn 累积的文件 diff(M5 中用于 commit 消息)。
    pub file_diffs: HashMap<std::path::PathBuf, String>,
    /// 本 turn 累计 token 用量(`AgentState` 每 turn 新建,故实为 turn 级;
    /// 会话级累计见 `NodeContext::session_usage`)。
    pub total_usage: TokenUsage,
    /// M8:最近一次 LLM 调用上报的 `input_tokens`(权威上下文大小信号)。
    /// `pre_loop` 把它透传给 compactor 作 `llm_reported_input_tokens`。
    /// 此前误用 `total_usage.input_tokens`(turn 内逐次 model_call 的
    /// **累计**值)—— 累计值随迭代数线性增长,长 turn 下(默认阈值
    /// 10k,3 次调用 × ~5k 即越线)compactor 会在每次迭代都误触发
    /// microcompact / smart_prune。由 `model_call` 在每次成功调用后置位,
    /// 每 turn 经 `AgentState::default()` 重置。
    pub last_llm_input_tokens: Option<u32>,
    /// LLM 近期输出的 content blocks(文本 + 工具调用)。
    pub latest_content: Vec<ContentBlock>,
    /// v1.x M2-fix:本回合内 `pre_loop` 一旦已用 `ctx.messages`(初始用户
    /// 输入)填充过 `state.messages` 即为 `true`。同回合内后续的
    /// `pre_loop` 调用**不得**清空 `state.messages` —— 否则 `model_call` /
    /// `tool_exec` 提交的助手工具调用与工具结果会被丢弃,模型每次迭代
    /// 都只看到原始问题(导致它循环重复同一工具调用)。
    pub history_seeded: bool,
    /// v1.x loop-guard: 上一轮 `tool_exec` 看到的「规范化调用签名」(name +
    /// args 的稳定字符串)与连续命中次数。当同一签名连续出现 ≥
    /// `REPEAT_LOOP_THRESHOLD` 次时,注入一条 system-reminder 提示模型停止
    /// 重复、基于已有结果作答。防御模型在历史已持久化后仍偶发的重复循环。
    pub last_call_signature: Option<String>,
    pub repeat_hit_count: u32,
    /// v1.x progress-nudge:本 turn 已发起的 web_fetch URL 序列(最近优先,
    /// 容量 `WEB_HISTORY_CAP`)。`tool_exec` 用它检测「同域名反复抓取」并
    /// 注入结构化事实摘要,对抗 step-3.7-flash 类模型在多源检索中无法收敛
    /// 的问题。
    pub web_fetch_history: Vec<WebFetchEntry>,
    /// v1.x progress-nudge:同 turn 内按域名计的 web_fetch 次数(用于快速
    /// 判断「google.com × 3」式循环)。
    pub web_domain_counts: HashMap<String, u32>,
    /// `StateGraph::run` 未发生致命错误而正常结束时置 `true`。
    /// `submission_loop` 据此决定是否 emit `TurnComplete`。
    pub completed_normally: bool,
    /// v1.2 P1-12:`model_call` 因会话级 token 预算耗尽而提前返回时置
    /// `true`。与 `completed_normally`(自然经 `CheckStop` 结束)区分,
    /// 让 `submission_loop` 能 emit 带 `TurnStatus::TokenBudgetExceeded`
    /// 的 `TurnComplete`,而不是静默丢弃回合。每回合经
    /// `AgentState::default` 重置。
    pub budget_exceeded: bool,
    /// `model_call` 触到 `max_iterations` 安全阀时置 `true`。
    /// `submission_loop` 据此把 turn 状态判为 `TurnStatus::MaxIterations`。
    /// 此前用 `iteration > 32` 硬编码判断,当配置上限 < 32(如 GAIA 的
    /// `REFLECT_MAX_ITERATIONS=20`)时会把"用尽迭代未作答"误报为 `Success`,
    /// 既掩盖真因、又丢失诊断信号。由 `model_call` 触发上限时置位。
    pub hit_max_iterations: bool,
    /// GAIA-fix:`model_call` 触顶 `max_iterations` 后进入"强制收口作答"
    /// 阶段时置 `true`。首次触顶时置位,并退回发一次
    /// **无工具**的收口调用(模型只能写文本,必然产出 `FINAL ANSWER:`);
    /// 下次再进 `model_call` 入口安全阀时据此**真正终止**。一次性 flag,
    /// 保证不会无限续作。由 `AgentState::default` 每 turn 重置。
    pub force_final_answer: bool,
    /// M4:本回合 LLM 可见的工具,按 `always_on ∪ active_skills` 过滤。
    /// 由 `pre_loop` 设置、`model_call` 读取。未配置 M4 依赖时为空。
    pub effective_tools: Vec<ToolSpec>,
    /// M4:system prompt 块(core + append),由 `pre_loop` 构造。
    /// `model_call` 读取它填充 `ChatRequest::system`。
    pub system_blocks: SystemBlocks,
    /// M4:临时提醒文本(以带 `<system-reminder>` 标签的 `User` 消息
    /// 渲染)。由 `pre_loop` 设置。
    pub ephemeral_text: String,
    /// M5:本会话内 compactor 最近一次产出的 LLM 摘要(若有)。下一次
    /// compaction 时喂给 `summarize_recent`,让 LLM 能做增量更新,
    /// 而不是从头重新总结整个对话。
    pub compaction_summary: Option<String>,
    /// v1.1.0 Phase 6 P0:本轮 pre_loop 收集到的"上下文恢复"元消息
    /// (active files / subagent registry / session memory notes)。
    /// `model_call` 在 ephemeral 推送之后再渲染成 `<system-reminder>`
    /// User 块,确保 LLM 看到的关键事实跨 turn / 跨 compact 一致。
    /// 每轮从 `M4Deps` 的共享源(`NoteStore` / `SubagentRegistry` /
    /// `ActiveFileRecovery`)重新计算,所以本字段不需跨 turn 持久化。
    pub recovery_meta: Vec<RecoveryEntry>,
    /// v1.x auto-continue:本 turn 因「输出被 provider 的 `max_tokens`
    /// 截断」而自动续作的次数。每次续作都把一条 User "请继续并收口"
    /// 消息追加进历史,然后回到 `PreLoop` 让模型补完上一次中断的作答。
    /// 受 `MAX_AUTO_CONTINUATIONS` 上限保护,避免无限续作;同时每次续作都
    /// 让 `iteration` +1,天然受 `max_iterations` 安全阀约束。每 turn 重置。
    pub auto_continue_count: u32,
}

/// v1.x progress-nudge:每条 web_fetch 的 URL + 200 字符片段摘要,供
/// `tool_exec` 在「同域名反复抓取 / 已收集事实充分」时构建结构化提示。
#[derive(Debug, Clone, Default)]
pub struct WebFetchEntry {
    pub url: String,
    /// 来自工具 content 第一个 text block 的前 240 字符(去尾空白)。
    pub snippet: String,
    pub bytes: u64,
}

impl AgentState {
    pub fn has_tool_calls(&self) -> bool {
        self.latest_content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}
