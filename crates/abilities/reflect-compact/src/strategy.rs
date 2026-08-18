//! `Compactor` —— 串联 microcompact → smart_prune → LLM-summarize 三级压缩。
//!
//! 对应 Reflect 的递进式压缩策略:
//! 1. 估算总 token 数。
//! 2. 若低于 `trigger * microcompact_trigger_ratio` → `Noop`。
//! 3. 执行 microcompact。若当前低于 `trigger` → 返回 `Microcompact`。
//! 4. 执行 smart_prune。若当前低于 `target` → 返回 `SmartPrune`。
//! 5. 若 `summarize_after` 为 true,调用 LLM 摘要器。若摘要结果低于
//!    `target` → 返回 `LlMSummarize`。
//! 6. 回退到 smart_prune 的尽力而为结果。
//!
//! 无环依赖:仅依赖 `reflect-llm` 的类型。具体 `Summarizer` 由调用方注入
//! (通常是在 `reflect-exec` 中构造的 `LlmSummarizer`)。

use std::sync::Arc;

use reflect_llm::ChatMessage;
use reflect_protocol::{ContextCompactedEvent, ContextCompactedStrategy};

use crate::microcompact::{MicrocompactConfig, microcompact};
use crate::smart_prune::{SmartPruneConfig, smart_prune};
use crate::summarizer::Summarizer;
use crate::tokens::estimate_messages;

/// 默认触发阈值。M5 v0 默认 `max_estimated_tokens = 10000`,以便在长对话
/// 中频繁触发 microcompact;此前的 M4 默认值为 160_000。运维人员可通过
/// `REFLECT_AUTO_COMPACT_INPUT_TOKENS` 环境变量覆盖。
pub const DEFAULT_TRIGGER_TOKENS: u32 = 10_000;

/// 压缩器的可调参数。
#[derive(Debug, Clone)]
pub struct CompactorConfig {
    /// 触发压缩的总 token 阈值。
    pub trigger_tokens: u32,
    /// 低于 `trigger * ratio` 时视为无需压缩(no-op)。
    pub microcompact_ratio: f32,
    /// microcompact 阶段的 `keep_recent`。
    pub keep_recent_microcompact: usize,
    /// smart_prune 阶段的 `keep_recent`。
    pub keep_recent_smart_prune: usize,
    /// smart_prune 阶段的 `target_tokens`(默认 = trigger * 0.75)。
    pub target_ratio: f32,
    /// smart_prune 之后仍超出预算时,是否调用 LLM 摘要器。
    pub summarize_after: bool,
}

impl Default for CompactorConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: DEFAULT_TRIGGER_TOKENS,
            microcompact_ratio: 0.7,
            keep_recent_microcompact: 30,
            keep_recent_smart_prune: 40,
            target_ratio: 0.75,
            summarize_after: true,
        }
    }
}

impl CompactorConfig {
    /// 由 `trigger * target_ratio` 计算 smart_prune 目标 token 数。
    pub fn target_tokens(&self) -> u32 {
        ((self.trigger_tokens as f32) * self.target_ratio) as u32
    }
}

/// 压缩编排器。持有配置与外部注入的摘要器。
pub struct Compactor {
    cfg: CompactorConfig,
    summarizer: Arc<dyn Summarizer>,
}

impl std::fmt::Debug for Compactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compactor")
            .field("cfg", &self.cfg)
            .field("summarizer", &"<dyn Summarizer>")
            .finish()
    }
}

impl Compactor {
    /// 使用给定配置和摘要器创建新压缩器。
    pub fn new(cfg: CompactorConfig, summarizer: Arc<dyn Summarizer>) -> Self {
        Self { cfg, summarizer }
    }

    /// 执行压缩。返回压缩后的消息列表与记录所选策略的
    /// `ContextCompactedEvent`。
    ///
    /// 事件包含 `removed_messages`、`before_tokens`、`after_tokens`。
    /// `strategy` 取值为 `Noop` / `Microcompact` / `SmartPrune` /
    /// `LlMSummarize` 之一。
    ///
    /// `prev_summary` 是上一次 LLM 生成的摘要(若有)—— 当为 `Some` 时,
    /// LLM-summarize 步骤调用 `summarize_recent` 而非 `summarize_full`,
    /// 产生增量更新而非从零重新摘要。
    pub async fn compact(
        &self,
        messages: Vec<ChatMessage>,
    ) -> (Vec<ChatMessage>, ContextCompactedEvent) {
        self.compact_with_prior_and_tokens(messages, None, None)
            .await
    }

    /// 完整入口。当 `llm_reported_input_tokens` 为 `Some(n)` 时,
    /// 其值取自 LLM 上一次 `Usage` 事件的输入 token 数(Anthropic 的
    /// `input_tokens` 已包含 `cache_creation` 子段,因此即为线上计费的
    /// 输入量)。当为 `None` 或小于本地估算时,使用本地
    /// `estimate_messages` 启发式作为保守回退。触发阈值取
    /// `max(estimate, llm_reported)`,因此**任一**信号越界都会触发——
    /// 这是安全的默认行为,因为:
    /// - 若 LLM 异常地报告 0,本地估算仍能捕获超大 prompt;
    /// - 若本地估算偏低(如尚未计入系统工具 + skills 目录),
    ///   LLM 报告的总量是权威值。
    pub async fn compact_with_prior_and_tokens(
        &self,
        messages: Vec<ChatMessage>,
        prev_summary: Option<&str>,
        llm_reported_input_tokens: Option<u32>,
    ) -> (Vec<ChatMessage>, ContextCompactedEvent) {
        let local_estimate = estimate_messages(&messages);
        let before_tokens = match llm_reported_input_tokens {
            Some(n) => n.max(local_estimate),
            None => local_estimate,
        };
        let micro_threshold =
            ((self.cfg.trigger_tokens as f32) * self.cfg.microcompact_ratio) as u32;

        // 第 1 步:低于 micro 阈值 → noop。
        if before_tokens < micro_threshold {
            return (
                messages,
                ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Noop,
                    removed_messages: 0,
                    before_tokens,
                    after_tokens: before_tokens,
                },
            );
        }

        // 第 2 步:microcompact。
        let mc_cfg = MicrocompactConfig {
            trigger_tokens: self.cfg.trigger_tokens,
            keep_recent: self.cfg.keep_recent_microcompact,
            trigger_ratio: self.cfg.microcompact_ratio,
        };
        let (after_mc, mc_report) = microcompact(messages, &mc_cfg);
        if !mc_report.was_compacted {
            // Microcompact 提前退出(例如 n <= keep_recent)。
            // 直接尝试 smart_prune。
        } else if estimate_messages(&after_mc) < self.cfg.trigger_tokens {
            let after_tokens = estimate_messages(&after_mc);
            return (
                after_mc,
                ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Microcompact,
                    removed_messages: mc_report.removed_count,
                    before_tokens,
                    after_tokens,
                },
            );
        }

        // 第 3 步:smart_prune。
        let sp_cfg = SmartPruneConfig {
            trigger_tokens: self.cfg.trigger_tokens,
            keep_recent: self.cfg.keep_recent_smart_prune,
            target_tokens: self.cfg.target_tokens(),
            ..Default::default()
        };
        let (after_sp, sp_report) = smart_prune(after_mc, &sp_cfg);
        let after_sp_tokens = estimate_messages(&after_sp);
        if after_sp_tokens < self.cfg.target_tokens() {
            return (
                after_sp,
                ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::SmartPrune,
                    removed_messages: sp_report.removed_count,
                    before_tokens,
                    after_tokens: after_sp_tokens,
                },
            );
        }

        // 第 4 步:LLM 摘要(若启用)。M5 v0:存在历史摘要时使用增量模式
        // (`summarize_recent`),否则使用全量模式。
        if self.cfg.summarize_after {
            let result = match prev_summary {
                Some(prev) => {
                    self.summarizer
                        .summarize_recent(&after_sp, Some(prev))
                        .await
                }
                None => self.summarizer.summarize_full(&after_sp).await,
            };
            match result {
                Ok(summary) => {
                    // summary 作为 System 消息存档历史;但**必须保留最近的
                    // 用户消息(及紧随其后的 assistant/tool 对)**,否则下一次
                    // `model_call` 的 `request.messages` 只剩一条 System 消息
                    // —— 它被 provider 放进 `system` 字段而非 `messages`,
                    // 导致 `messages` 为空,上游 API 返回
                    // `messages must not be empty`(GAIA 截断类失败的直接原因)。
                    // 修复:summary 之后追加 smart_prune 的尾部 `keep_recent`
                    // 条消息(含最新用户问题 + 最近工具结果),让对话能续作。
                    let mut summary_msg = vec![ChatMessage::System(format!(
                        "<summary>\n{summary}\n</summary>"
                    ))];
                    let tail = keep_recent_tail(&after_sp, self.cfg.keep_recent_smart_prune);
                    summary_msg.extend(tail);
                    let after_tokens = estimate_messages(&summary_msg);
                    return (
                        summary_msg,
                        ContextCompactedEvent {
                            strategy: ContextCompactedStrategy::LlMSummarize,
                            removed_messages: sp_report.removed_count + 1,
                            before_tokens,
                            after_tokens,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(?e, "summarize failed; returning best-effort smart_prune");
                }
            }
        }

        // 回退:返回 smart_prune 的结果。
        (
            after_sp,
            ContextCompactedEvent {
                strategy: ContextCompactedStrategy::SmartPrune,
                removed_messages: sp_report.removed_count,
                before_tokens,
                after_tokens: after_sp_tokens,
            },
        )
    }
}

/// 返回 `messages` 的末尾 `n` 条,但保证至少包含最后一条 User 消息
/// (即原始用户问题)。LLM-summarize 后必须保留这一条,否则 provider 端
/// `messages` 为空会被 API 拒绝(`messages must not be empty`)。
///
/// 截断点对齐:若末尾 `n` 条的第一条是 Tool 消息(dangling tool result,
/// 缺少对应 assistant tool_use),向前回溯一条以保持成对完整。
fn keep_recent_tail(messages: &[ChatMessage], n: usize) -> Vec<ChatMessage> {
    if messages.is_empty() || n == 0 {
        return Vec::new();
    }
    // 找到最后一条 User 消息的位置 —— 必须保留它(原始问题)。
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m, ChatMessage::User(_)))
        .unwrap_or(0);
    // 起点 = min(末尾 n 条的起点, 最后一条 User 的位置)。
    let mut start = messages.len().saturating_sub(n).min(last_user);
    // 若起点落在 dangling Tool 消息上(无对应 assistant tool_use),
    // 向前回溯一条让它与 assistant 成对。
    while start > 0 && matches!(messages[start], ChatMessage::Tool(_)) {
        start -= 1;
    }
    messages[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use reflect_llm::{AssistantContent, ContentBlock, ToolResult, UserContent};

    /// 返回预制输出的 mock summarizer。
    struct MockSummarizer {
        canned: String,
        fail: bool,
    }

    #[async_trait]
    impl Summarizer for MockSummarizer {
        async fn summarize_full(
            &self,
            _: &[ChatMessage],
        ) -> Result<String, crate::summarizer::SummarizerError> {
            if self.fail {
                Err(crate::summarizer::SummarizerError::Cancelled)
            } else {
                Ok(self.canned.clone())
            }
        }
        async fn summarize_recent(
            &self,
            _: &[ChatMessage],
            _: Option<&str>,
        ) -> Result<String, crate::summarizer::SummarizerError> {
            self.summarize_full(&[]).await
        }
    }

    fn huge_message_list() -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage::System("sys".into())];
        for i in 0..200 {
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("c{i}"),
                content: vec![ContentBlock::Text {
                    text: "x".repeat(1000),
                }],
                is_error: false,
            }));
        }
        msgs
    }

    #[tokio::test]
    async fn noop_when_under_micro_threshold() {
        let c = Compactor::new(
            CompactorConfig::default(),
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })];
        let (_, evt) = c.compact(msgs).await;
        assert!(matches!(evt.strategy, ContextCompactedStrategy::Noop));
    }

    #[tokio::test]
    async fn microcompact_when_over_threshold() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (out, evt) = c.compact(msgs).await;
        // tokens ≈ 200k,threshold = 100 → 明显超限。
        // 200 条工具结果、keep_recent=5 时,microcompact 会替换大多数
        // 旧工具结果。若能压到 trigger_tokens=100 以下,返回 Microcompact;
        // 否则升级策略。
        let strat = evt.strategy;
        assert!(matches!(
            strat,
            ContextCompactedStrategy::Microcompact
                | ContextCompactedStrategy::SmartPrune
                | ContextCompactedStrategy::LlMSummarize
        ));
        assert!(out.len() <= 205);
    }

    #[tokio::test]
    async fn llm_summarize_when_smart_prune_insufficient() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01, // very tight target so smart_prune fails
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: "fake summary".into(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_, evt) = c.compact(msgs).await;
        // 目标紧 → 升级到 summarize。
        assert!(matches!(
            evt.strategy,
            ContextCompactedStrategy::LlMSummarize
        ));
    }

    #[tokio::test]
    async fn fallback_to_smart_prune_when_summarize_fails() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: true,
            }),
        );
        let msgs = huge_message_list();
        let (_, evt) = c.compact(msgs).await;
        // Summarizer 失败 → 返回 best-effort 的 smart_prune。
        assert!(matches!(evt.strategy, ContextCompactedStrategy::SmartPrune));
    }

    #[test]
    fn target_tokens_is_trigger_times_ratio() {
        let cfg = CompactorConfig {
            trigger_tokens: 200_000,
            target_ratio: 0.75,
            ..Default::default()
        };
        assert_eq!(cfg.target_tokens(), 150_000);
    }

    // ── GAIA-fix: LLM-summarize 保留尾部消息(含用户问题) ───────────────

    fn sample_conversation() -> Vec<ChatMessage> {
        // System + User(问题) + Assistant(tool_use) + Tool(result) + ...
        vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("What is 2+2?")],
            }),
            ChatMessage::Assistant(AssistantContent {
                text: Some("let me compute".into()),
                tool_calls: vec![reflect_llm::ToolCallRequest {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
                thinking: None,
            }),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text("4")],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("now what is 3+3?")],
            }),
            ChatMessage::Assistant(AssistantContent {
                text: Some("let me compute again".into()),
                tool_calls: vec![reflect_llm::ToolCallRequest {
                    id: "c2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
                thinking: None,
            }),
            ChatMessage::Tool(ToolResult {
                call_id: "c2".into(),
                content: vec![ContentBlock::text("6")],
                is_error: false,
            }),
        ]
    }

    #[test]
    fn keep_recent_tail_preserves_last_user_message() {
        // n=2 但最后一条 User 在更早位置 → 必须至少保留到那条 User。
        let msgs = sample_conversation();
        let tail = keep_recent_tail(&msgs, 2);
        // tail 必须包含最后一条 User("now what is 3+3?")。
        assert!(
            tail.iter().any(|m| matches!(m, ChatMessage::User(u)
                if u.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("3+3"))))
            ),
            "tail must include the last user message, got {:?}",
            tail.iter().map(|m| match m {
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
                ChatMessage::System(_) => "System",
            }).collect::<Vec<_>>()
        );
    }

    #[test]
    fn keep_recent_tail_backtracks_dangling_tool_result() {
        // n=3 让起点落在 Tool(c1) 上 → 必须向前回溯到 Assistant(c1)。
        let msgs = sample_conversation();
        let tail = keep_recent_tail(&msgs, 3);
        // tail 的第一条不应是 Tool(dangling)。
        assert!(
            !tail
                .first()
                .is_some_and(|m| matches!(m, ChatMessage::Tool(_))),
            "tail must not start with a dangling Tool result"
        );
    }

    #[test]
    fn keep_recent_tail_empty_input() {
        assert!(keep_recent_tail(&[], 5).is_empty());
        assert!(keep_recent_tail(&sample_conversation(), 0).is_empty());
    }

    // ── M8 P0b: LLM-reported input tokens drive trigger ─────────────────

    /// `huge_message_list` 本地估算约 200k token。trigger=10k 时,
    /// compactor 触发(无论 LLM 信号如何)。这是 M7 基线 —— 不得回归。
    #[tokio::test]
    async fn huge_list_fires_compact_without_llm_signal() {
        let c = Compactor::new(
            CompactorConfig::default(),
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_out, evt) = c.compact_with_prior_and_tokens(msgs, None, None).await;
        assert!(!matches!(evt.strategy, ContextCompactedStrategy::Noop));
    }

    /// 本地估算 = 1000(仅 `huge_message_list` 的 1 条 system 消息 ≈ 1
    /// token),LLM 上报 = 50_000,trigger = 10_000 → 必须触发,
    /// 因为 LLM 上报值是权威信号。
    #[tokio::test]
    async fn llm_reported_triggers_when_local_estimate_is_small() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        // 本地估算仅 1 token。
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })];
        let (_out, evt) = c
            .compact_with_prior_and_tokens(msgs, None, Some(50_000))
            .await;
        assert!(
            !matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "LLM-reported 50k over trigger 10k should fire, got strategy {:?}",
            evt.strategy
        );
    }

    /// 本地估算 = 50_000(大列表),LLM 上报 = 8_000,
    /// trigger = 10_000 → 必须触发,因为 `max(estimate, llm_reported)
    /// = 50_000 > 10_000`。LLM 上报值**不**允许压制真实的本地超限。
    #[tokio::test]
    async fn max_of_estimate_and_llm_triggers_when_estimate_is_big() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_out, evt) = c
            .compact_with_prior_and_tokens(msgs, None, Some(8_000))
            .await;
        assert!(
            !matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "estimate 50k over trigger 10k must still fire even with llm=8k, got strategy {:?}",
            evt.strategy
        );
    }

    /// 本地估算 > micro-threshold(trigger * 0.7),LLM 上报 = 0
    /// (异常),trigger = 10_000 → 必须触发,因为本地估算
    /// 防 LLM 误报。
    #[tokio::test]
    async fn llm_reported_zero_falls_back_to_local_estimate() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        // 构造一个本地估算落在 micro-threshold(trigger * 0.7 = 7_000)
        // 之上的中等消息列表。每条工具结果 4000 字符 ≈ 1143 token
        // × 20 = 22_860 token。
        let mut msgs: Vec<ChatMessage> = vec![ChatMessage::System("s".repeat(100))]; // ~29 tokens
        for i in 0..20 {
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("c{i}"),
                content: vec![ContentBlock::Text {
                    text: "x".repeat(4000),
                }],
                is_error: false,
            }));
        }
        // 自检:本地估算应 > micro-threshold(7000)。
        let local = estimate_messages(&msgs);
        assert!(
            local > 7000,
            "test setup wrong: local estimate {local} should exceed micro-threshold 7000"
        );
        let (_out, evt) = c.compact_with_prior_and_tokens(msgs, None, Some(0)).await;
        assert!(
            !matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "llm=0 must not suppress local-estimate over-budget, got strategy {:?}",
            evt.strategy
        );
    }
}
