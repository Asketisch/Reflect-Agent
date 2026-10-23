//! `models` — 讨论协议的核心数据类型。
//!
//! 包含 4 个 ID / 标识类型(`DiscussionId` / `MessageId` / `AgentId`),
//! 2 个枚举(`DiscussionMode` / `MessageKind`),1 个消息结构 `DiscussionMessage`,
//! 1 个配置 `DiscussionConfig`,1 个结果 `DiscussionResult`。
//!
//! 所有公开类型都 `#[derive(Serialize, Deserialize)]` 以便持久化到 JSONL;
//! serde 用 `#[serde(rename_all = "snake_case")]` 保持 wire 兼容。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 讨论会话 ID(由 `DiscussionOrchestrator::new` 生成)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscussionId(pub Uuid);

impl DiscussionId {
    /// 生成新的讨论 ID(v4 UUID)。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DiscussionId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<DiscussionId> for Uuid {
    fn from(d: DiscussionId) -> Uuid {
        d.0
    }
}

impl std::fmt::Display for DiscussionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 消息 ID(在 bus 内单调递增,per discussion)。
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MessageId(pub u64);

/// 参与讨论的 Agent 标识 = `SubAgentSpec.role`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    /// 字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// `call_<role>` —— 跟 [`reflect_subagent::spec::SubAgentSpec::tool_name()`] 保持一致。
    pub fn tool_name(&self) -> String {
        format!("call_{}", self.0)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 讨论执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscussionMode {
    /// 编排器按轮次串行调用每个 Agent(每轮一个 agent 完成后再下一个)。
    Sequential,
    /// 所有 Agent 并发,每轮收齐响应后再开下一轮(`tokio::task::JoinSet`)。
    Concurrent,
}

/// 消息类型。
///
/// `Consensus` 标志 Agent 自报"我已达成共识";`Finish` 标志 Agent 主动结束讨论
/// (等效于调用 `finish_discussion` 工具);`Utterance` 是普通发言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// 普通发言。
    Utterance,
    /// Agent 标记自己已达成共识。
    Consensus,
    /// Agent 主动结束讨论。
    Finish,
}

/// 一条讨论消息。
///
/// `recipients` 为空时表示**广播**给所有非 sender 的 agent;非空时表示**单播**到指定
/// agent 列表。`round` 字段记录该消息是第几轮(0-based)发出的,`token_usage` 由
/// orchestrator 在每轮末注入以支持 `pricing.rs` 后续计费。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionMessage {
    /// bus 内单调递增的 message id。
    pub id: MessageId,
    /// 所属讨论 id。
    pub discussion_id: DiscussionId,
    /// 发送方。
    pub from: AgentId,
    /// 消息类型。
    pub kind: MessageKind,
    /// 文本内容。
    pub content: String,
    /// 接收方列表;空 = 广播给所有其他 agent。
    pub recipients: Vec<AgentId>,
    /// 该消息发出时所在的轮次(0-based)。
    pub round: u32,
    /// LLM-reported token 累计(per-provider;预留 M9 计费用)。
    ///
    /// v0.2.4 起:在 LLM 集成路径下由 `prompt_for_closure` 通过
    /// `SendMessageTool.token_usage` 槽写入;non-LLM 路径下保持空 map。
    /// 通过 [`DiscussionMessage::token_usage_as_protocol`] 在 protocol
    /// 边界转换为 `reflect_protocol::TokenUsage`。
    #[serde(default)]
    pub token_usage: BTreeMap<String, u32>,
}

impl DiscussionMessage {
    /// v0.2.4: 把 per-provider 灵活的 `BTreeMap` 转为 protocol 边界上统一的
    /// `reflect_protocol::TokenUsage`。仅在 `input` / `output` 任一非零时
    /// 返回 `Some`;`cached` / `cache_write` 按同名 key 取值,缺失按 0。
    /// 其他未知 key 静默丢弃 —— 这是边界行为,在 helper 的 rustdoc 里固化。
    pub fn token_usage_as_protocol(&self) -> Option<reflect_protocol::TokenUsage> {
        let input = self.token_usage.get("input").copied().unwrap_or(0);
        let output = self.token_usage.get("output").copied().unwrap_or(0);
        let cached = self.token_usage.get("cached").copied().unwrap_or(0);
        let cache_write = self.token_usage.get("cache_write").copied().unwrap_or(0);
        if input == 0 && output == 0 {
            return None;
        }
        Some(reflect_protocol::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: cache_write,
            total_tokens: input + output,
        })
    }
}

/// 讨论配置。
///
/// `consensus_window` 控制"多少轮内所有 participant 都发过 `Consensus` 才算达成":
/// 默认 1 表示"当前轮所有人都发 Consensus 即达成";`max_rounds` 是硬上限,
/// 超过则强制返回 [`DiscussionResult::NoConsensus`];`mailbox_capacity` 是每个
/// agent mailbox 的 `mpsc` channel 容量上限(防 OOM)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionConfig {
    pub mode: DiscussionMode,
    pub participants: Vec<AgentId>,
    /// 初始问题/任务描述(注入到首轮 system prompt)。
    pub topic: String,
    /// 共识窗口大小(0 = 关闭共识检测,等同 `max_rounds` 强制结束)。
    pub consensus_window: u32,
    /// 最大轮数;超过则强制结束并返回 [`DiscussionResult::NoConsensus`]。
    pub max_rounds: u32,
    /// Mailbox 容量上限(per agent,防 OOM)。
    pub mailbox_capacity: usize,
    /// v1.4 C3:裁判模式 —— 每轮结束由裁判(独立 LLM)通读全部发言出
    /// 结构化裁决,`agreed` 才算共识;代理自报 Consensus 降级为裁判的
    /// 参考信号之一。`false`(默认)维持纯自报共识的历史行为。
    #[serde(default)]
    pub judge: bool,
}

impl Default for DiscussionConfig {
    fn default() -> Self {
        Self {
            mode: DiscussionMode::Concurrent,
            participants: vec![],
            topic: String::new(),
            consensus_window: 1,
            max_rounds: 10,
            mailbox_capacity: 64,
            judge: false,
        }
    }
}

/// v1.4 C3:裁判裁决(结构化输出,由裁判 LLM 产出 JSON 解析而来)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// 是否已达共识。
    pub agreed: bool,
    /// 当前共识摘要(agreed 时)或当前分歧焦点(未达成时)。
    pub summary: String,
    /// 未达共识时的待解决点(裁判在下一轮注入,引导讨论收敛)。
    #[serde(default)]
    pub blockers: Vec<String>,
}

impl JudgeVerdict {
    /// 从裁判 LLM 的自由文本产出解析裁决:容错提取首个 `{...}` 块
    /// (容忍 markdown fence / 前后噪声),解析失败返回 Err。
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let stripped = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed)
            .trim()
            .trim_end_matches("```")
            .trim();
        let json_str = match (stripped.find('{'), stripped.rfind('}')) {
            (Some(s), Some(e)) if s <= e => &stripped[s..=e],
            _ => stripped,
        };
        let v: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("invalid JSON: {e}; raw: {text}"))?;
        let agreed = v
            .get("agreed")
            .and_then(|x| x.as_bool())
            .ok_or_else(|| format!("missing 'agreed' field; raw: {text}"))?;
        let summary = v
            .get("summary")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let blockers = v
            .get("blockers")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            agreed,
            summary,
            blockers,
        })
    }
}

/// v1.4 C3:裁判回调 —— 每轮结束后由 runtime 调用,入参(当前轮次,
/// 全量 transcript),产出裁决。错误字符串仅用于 warn 日志(裁判故障时
/// 回退到自报共识路径,不中断讨论)。
pub type JudgeFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<JudgeVerdict, String>> + Send>>;
pub type JudgeCallback = dyn Fn(u32, Vec<DiscussionMessage>) -> JudgeFuture + Send + Sync;

/// 讨论最终结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DiscussionResult {
    /// 共识达成;`final_round` 是达成时的轮次,`summary` 是最后一条 consensus 消息内容。
    Consensus { final_round: u32, summary: String },
    /// 达到 `max_rounds` 仍未共识;`rounds_completed` 是实际跑的轮数,`transcript_len`
    /// 给调用方参考 transcript 体量。
    NoConsensus {
        rounds_completed: u32,
        transcript_len: usize,
    },
    /// 某个 Agent 主动调用 `finish_discussion` 工具结束讨论;`by` 是触发者。
    Finished { by: AgentId, final_round: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DiscussionId(讨论 id)───────────────────────────────

    #[test]
    fn discussion_id_is_unique() {
        let a = DiscussionId::new();
        let b = DiscussionId::new();
        assert_ne!(a, b, "v4 UUIDs should be unique across calls");
    }

    #[test]
    fn discussion_id_default_is_random() {
        // default() == new(),所以两次 default 也应该不同
        assert_ne!(DiscussionId::default(), DiscussionId::default());
    }

    // ── AgentId(代理 id)────────────────────────────────────

    #[test]
    fn agent_id_tool_name_matches_subagent_convention() {
        // 跟 `reflect_subagent::spec::SubAgentSpec::tool_name()` 保持一致:`call_<role>`
        let id = AgentId("explorer".into());
        assert_eq!(id.tool_name(), "call_explorer");
        assert_eq!(
            AgentId("advocate-v2".into()).tool_name(),
            "call_advocate-v2"
        );
    }

    #[test]
    fn agent_id_serde_roundtrip() {
        let id = AgentId("moderator".into());
        let j = serde_json::to_string(&id).unwrap();
        // transparent newtype 序列化就是字符串
        assert_eq!(j, "\"moderator\"");
        let back: AgentId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, id);
    }

    // ── DiscussionMode / MessageKind(模式与消息类型)──────

    #[test]
    fn discussion_mode_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&DiscussionMode::Sequential).unwrap(),
            "\"sequential\""
        );
        assert_eq!(
            serde_json::to_string(&DiscussionMode::Concurrent).unwrap(),
            "\"concurrent\""
        );
        let s = "\"concurrent\"";
        let back: DiscussionMode = serde_json::from_str(s).unwrap();
        assert_eq!(back, DiscussionMode::Concurrent);
    }

    #[test]
    fn message_kind_serde_uses_snake_case() {
        for (kind, expected) in [
            (MessageKind::Utterance, "\"utterance\""),
            (MessageKind::Consensus, "\"consensus\""),
            (MessageKind::Finish, "\"finish\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), expected);
        }
    }

    // ── DiscussionConfig / Message / Result(配置/消息/结果)─

    #[test]
    fn discussion_config_default_field_values() {
        let c = DiscussionConfig::default();
        assert_eq!(c.mode, DiscussionMode::Concurrent);
        assert!(c.participants.is_empty());
        assert_eq!(c.topic, "");
        assert_eq!(c.consensus_window, 1);
        assert_eq!(c.max_rounds, 10);
        assert_eq!(c.mailbox_capacity, 64);
    }

    #[test]
    fn discussion_message_serde_roundtrip() {
        let msg = DiscussionMessage {
            id: MessageId(42),
            discussion_id: DiscussionId::new(),
            from: AgentId("advocate".into()),
            kind: MessageKind::Utterance,
            content: "I think async is better.".into(),
            recipients: vec![],
            round: 0,
            token_usage: BTreeMap::from([("input".into(), 100), ("output".into(), 50)]),
        };
        let j = serde_json::to_string(&msg).unwrap();
        let back: DiscussionMessage = serde_json::from_str(&j).unwrap();
        assert_eq!(back, msg);
        // BTreeMap 序列化顺序稳定(按 key 升序)
        assert!(j.find("\"input\":100").unwrap() < j.find("\"output\":50").unwrap());
    }

    /// v0.2.4: `token_usage_as_protocol` 把 per-provider BTreeMap 翻译成
    /// `reflect_protocol::TokenUsage`;只有 input/output 非零时才返回 Some。
    #[test]
    fn token_usage_as_protocol_converts_btreemap() {
        let mut map = BTreeMap::new();
        map.insert("input".to_string(), 100u32);
        map.insert("output".to_string(), 50);
        map.insert("cached".to_string(), 7);
        map.insert("cache_write".to_string(), 3);
        map.insert("exotic_provider_key".to_string(), 999);
        let msg = DiscussionMessage {
            id: MessageId(0),
            discussion_id: DiscussionId::new(),
            from: AgentId("a".into()),
            kind: MessageKind::Utterance,
            content: String::new(),
            recipients: vec![],
            round: 0,
            token_usage: map,
        };
        let u = msg
            .token_usage_as_protocol()
            .expect("input/output nonzero → Some");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cached_tokens, 7);
        assert_eq!(u.cache_write_tokens, 3);
        assert_eq!(u.total_tokens, 150);
    }

    #[test]
    fn token_usage_as_protocol_returns_none_when_empty() {
        let msg = DiscussionMessage {
            id: MessageId(0),
            discussion_id: DiscussionId::new(),
            from: AgentId("a".into()),
            kind: MessageKind::Utterance,
            content: String::new(),
            recipients: vec![],
            round: 0,
            token_usage: BTreeMap::new(),
        };
        assert!(
            msg.token_usage_as_protocol().is_none(),
            "empty map must yield None"
        );
    }

    #[test]
    fn discussion_result_serde_tag_distinguishes_variants() {
        // 三个变体的 JSON 通过 `outcome` tag 区分
        let r1 = DiscussionResult::Consensus {
            final_round: 2,
            summary: "agreed".into(),
        };
        let j1 = serde_json::to_string(&r1).unwrap();
        assert!(j1.contains(r#""outcome":"consensus""#), "got: {j1}");
        assert!(j1.contains(r#""final_round":2"#));

        let r2 = DiscussionResult::NoConsensus {
            rounds_completed: 5,
            transcript_len: 15,
        };
        let j2 = serde_json::to_string(&r2).unwrap();
        assert!(j2.contains(r#""outcome":"no_consensus""#), "got: {j2}");

        let r3 = DiscussionResult::Finished {
            by: AgentId("moderator".into()),
            final_round: 1,
        };
        let j3 = serde_json::to_string(&r3).unwrap();
        assert!(j3.contains(r#""outcome":"finished""#), "got: {j3}");

        // 序列化往返(roundtrip)
        let back1: DiscussionResult = serde_json::from_str(&j1).unwrap();
        let back2: DiscussionResult = serde_json::from_str(&j2).unwrap();
        let back3: DiscussionResult = serde_json::from_str(&j3).unwrap();
        assert_eq!(back1, r1);
        assert_eq!(back2, r2);
        assert_eq!(back3, r3);
    }
}
