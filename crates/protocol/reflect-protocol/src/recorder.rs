//! Rollout 录制契约(M5)。
//!
//! `reflect-core` 在每个值得记录的事件上 emit `RolloutRecord`,由某个
//! `RolloutRecorder` 实现负责持久化。trait 放在 `reflect-protocol`,这样
//! `reflect-core` 可持有 `Option<Arc<dyn RolloutRecorder>>` 而无需依赖
//! `reflect-rollout`(否则会引入 `core ↔ rollout` 循环依赖)。
//!
//! 具体的 `JsonlRolloutWriter` 实现位于 `reflect-rollout` crate;
//! 测试与短期运行可使用 `NullRecorder`。

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::TokenUsage;
use crate::item::{PermissionMode, PlanId, ThreadId, TurnId};

/// 标识持久化消息来自对话的哪一方。
///
/// 故意保持最小化 —— `content` 负载以 `serde_json::Value` 承载实际数据,
/// 更丰富的结构体留在 `reflect-core` 中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// CLI `resume` 流与会话索引用到的轻量会话元数据。`message_count` 在每条
/// 记录写入时延迟更新。
///
/// v1.x: 新增 `input_tokens` / `output_tokens` / `total_tokens` /
/// `cost_usd` 四个字段,从每条 `RolloutRecord::TokenCount` 聚合。
/// 因 `cost_usd: Option<f64>` 而 `f64` 不实现 `Eq`,这里不再 derive
/// `Eq`(保留 `PartialEq`,足以满足 `assert_eq!`)。本类型从不作为
/// `HashMap` / `HashSet` 的 key(只作 value),移除 `Eq` 不影响任何调用方。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: ThreadId,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub message_count: usize,
    /// v1.x:会话标题(从首条 User 消息预览生成,见
    /// [`derive_title`])。`None` = 无首条 user 文本(空会话 / 旧文件)。
    /// 用户 `/rename` 的自定义名优先级更高(TUI 层先读 .name 文件再回退
    /// 本字段)。`#[serde(default)]` 保证旧 JSONL / 旧 reader 反序列化不破。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// v1.x: 整 session 累计 input tokens(聚合自 `RolloutRecord::TokenCount`)。
    /// 旧 session jsonl 中无 TokenCount 记录 → 反序列化为 0。
    #[serde(default)]
    pub input_tokens: u64,
    /// v1.x: 整 session 累计 output tokens。
    #[serde(default)]
    pub output_tokens: u64,
    /// v1.x: 整 session 累计 total tokens(= input + output;cached /
    /// cache_write 是 input 子集,刻意不重复加)。`u64` 防长会话溢出。
    #[serde(default)]
    pub total_tokens: u64,
    /// v1.x: 整 session 累计 USD cost(从每条 TokenCount 的 cost_usd 求和)。
    /// `None` = 没有 TokenCount 记录,或 model 不在 pricing 表里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 一条在线程 JSONL rollout 中持久化的事件。
///
/// 以 serde tagged union 形式序列化(`{"type": "...", ...}`),
/// 让外部工具可直接用 `jq` 处理文件而无需解析完整 Rust 枚举。
///
/// v1.x: 新增 `TokenCount` 变体携带 `cost_usd: Option<f64>`,导致 `f64`
/// 进入该 enum,故不再 derive `Eq`(保留 `PartialEq` 已足够)。本枚举从不
/// 作为 map/set key,移除 `Eq` 不破坏任何调用方。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutRecord {
    /// 每个线程恰好发送一次,先于所有其它记录。
    SessionMeta {
        session_id: ThreadId,
        model: String,
        started_at: DateTime<Utc>,
    },
    /// 一条持久化对话回合。`content` 是对 JSON 不透明的负载,让 recorder
    /// 既能携带 `ContentBlock`,也能携带纯文本或工具 payload。
    Message {
        turn_id: TurnId,
        role: MessageRole,
        content: serde_json::Value,
    },
    /// 由 `pre_loop` 在 `Compactor::compact` 执行完且策略不为 `Noop` 时发出。
    /// `summary` 是替换被丢弃切片的那条合成 `<summary>` System 消息正文。
    Compaction {
        turn_id: TurnId,
        strategy: String,
        removed_count: usize,
        summary: String,
    },
    /// 由 `SubAgentFactory::fork` 发出,标记一个分支会话。
    Fork {
        parent_session_id: ThreadId,
        branch_name: String,
    },
    /// v1.2 P0-3:`CheckpointTool` 在 `git_auto_commit` 后发出,记录工作区
    /// 的 git 快照 sha。`rewind` 工具据此把工作区 `git reset --hard` 回该
    /// sha。append-only:不截断历史,只追加 marker。`checkpoint_id` 是
    /// 工具返回给 LLM 的稳定标识(此处 = sha,但保留独立字段以便未来用
    /// uuid 而 sha 仅作恢复目标)。
    Checkpoint {
        turn_id: TurnId,
        /// 工作区 git 快照的 commit sha(`git rev-parse HEAD`)。
        sha: String,
        /// 给人 / LLM 的标签(可选)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        created_at: DateTime<Utc>,
    },
    /// v1.2 P0-3:`RewindTool` 在 `git_reset_hard` 后发出,记录回退到的
    /// 目标 checkpoint。append-only:会话历史保留,仅工作区文件回退。
    Rewind {
        turn_id: TurnId,
        /// 回退到的目标 checkpoint 的 sha。
        target_sha: String,
        /// 回退前的 HEAD sha(便于审计 / 再次前进)。
        from_sha: String,
        at: DateTime<Utc>,
    },
    /// M9: `DiscussionOrchestrator` 在讨论结束(达成共识 / 主动结束 /
    /// 达到最大轮数)时 emit 一份完整讨论 transcript。transcript payload
    /// 采用不透明 JSON,让 `reflect-protocol` 不依赖 `reflect-discussion`
    /// (避免 `core ↔ discussion` 循环依赖)。`reflect-rollout::redact`
    /// 仍对其施加 16 KiB 的内容截断上限。
    ///
    /// v0.2.4:整段讨论 transcript 对应 `agent_id = None`,每个 spawn 后
    /// 由 `prompt_for_closure` 产生的 per-agent 切片对应
    /// `Some(agent_id)`。`#[serde(default, skip_serializing_if)]` 保持
    /// 与 M9 记录的 wire 兼容(M9 记录无此字段)。
    DiscussionTranscript {
        discussion_id: Uuid,
        mode: String,
        participants: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        transcript: serde_json::Value,
    },
    /// v1.x: 每次 LLM 调用后的 per-turn usage 快照。
    /// 派生来源是 `EventMsg::TokenCount` —— 这里只把已 emit 的事件
    /// 持久化一遍,确保进程退出后 CLI `reflect session ls/show` 仍能看到累计。
    /// `cost_usd` 在 emit 当时已由 `model_call` 调用
    /// `reflect_llm::providers::price()` 算好;此处直接复用,不重新计算。
    TokenCount {
        turn_id: TurnId,
        /// 标准 5 段 usage: input / output / cached / cache_write / total。
        /// `cached` 与 `cache_write` 是 `input` 的子集,刻意不进 `total`。
        usage: TokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        at: DateTime<Utc>,
    },
    /// v1.x Plan mode:agent 请求进入 Plan mode。镜像 `PlanRequestEvent`,
    /// 但带上 `plan_id`(dispatch 时生成)与 `at`,让 session JSONL 能独立
    /// 还原 plan 生命周期,不依赖内存事件。
    PlanRequest {
        plan_id: PlanId,
        task: String,
        at: DateTime<Utc>,
    },
    /// v1.x Plan mode:agent 调研完成,plan markdown 已生成。**双写**:
    /// markdown 全文进 JSONL(session 完整可还原、`/export` 可导出),
    /// `path` 指向 `.reflect/plan/<plan_id>.md`(LLM `cat` 引用、
    /// `PlanReadyEvent.path` 契约不破)。两者用途不同,互不替代。
    PlanReady {
        plan_id: PlanId,
        markdown: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        at: DateTime<Utc>,
    },
    /// v1.x Plan mode:用户拒绝 / 要求 revise。镜像 `PlanRejectedEvent`,
    /// 与 `PlanReady` 同 `plan_id` 配对。
    PlanRejected {
        plan_id: PlanId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        at: DateTime<Utc>,
    },
    /// v1.x:PermissionMode 状态机切换通知。覆盖 plan 进入/退出、
    /// `/mode` slash、`Bypass → Prompt` 降级等所有路径。与
    /// `PermissionModeChangedEvent` 同构,但持久化到 JSONL 后让
    /// session 记录里可见完整的权限状态轨迹。
    PermissionModeChanged {
        from: PermissionMode,
        to: PermissionMode,
        at: DateTime<Utc>,
    },
}

impl RolloutRecord {
    /// `Message` 变体的便捷构造函数。
    pub fn message(turn_id: TurnId, role: MessageRole, content: serde_json::Value) -> Self {
        RolloutRecord::Message {
            turn_id,
            role,
            content,
        }
    }

    /// `SessionMeta` 的便捷构造函数。
    pub fn session_meta(session_id: ThreadId, model: impl Into<String>) -> Self {
        RolloutRecord::SessionMeta {
            session_id,
            model: model.into(),
            started_at: Utc::now(),
        }
    }
}

/// v1.x:从一段原始文本(通常是会话首条 user 消息)派生一个简短标题。
///
/// 规则:
/// - 折叠连续空白(含换行)为单个空格,去掉首尾空白;
/// - 按 **字符**(非字节)截到 [`TITLE_MAX_CHARS`] 个,超出加省略号 `…`;
/// - 空文本(只含空白)返回 `None`(调用方据此保持 `title = None`)。
///
/// 不调 LLM、即时、幂等 —— 适合在 `list_sessions` 扫描时对每个文件即时
/// 生成。用户 `/rename` 的自定义名在 TUI 层优先于本派生值。
pub fn derive_title(text: &str) -> Option<String> {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= TITLE_MAX_CHARS {
        return Some(trimmed.to_string());
    }
    let head: String = chars.into_iter().take(TITLE_MAX_CHARS).collect();
    Some(format!("{head}…"))
}

/// [`derive_title`] 的最大字符数(视觉宽度,非字节)。
pub const TITLE_MAX_CHARS: usize = 48;

/// [`RolloutRecord`] 的可插拔持久化后端。
///
/// `Send + Sync` 使其可挂在 `NodeContext` 上;`Debug` 使其在错误路径中
/// 可被 `{:?}` 美化打印。使用 `async_trait` 是为了 dyn 兼容性
/// (Rust 2024 stable 的 trait 中 `async fn` 尚不具备 dyn 兼容性)。
#[async_trait]
pub trait RolloutRecorder: Send + Sync + std::fmt::Debug {
    /// 追加一条记录到底层存储。实现应当遵循 best-effort:写入失败仅
    /// 记录日志并继续,避免磁盘满等错误中断正在进行的 turn。
    async fn record(&self, r: RolloutRecord) -> anyhow::Result<()>;

    /// 重放 `session_id` 的全部记录。格式错误的行应以 `tracing::warn!`
    /// 跳过,不应中断整体 replay。
    async fn replay(&self, session_id: ThreadId) -> anyhow::Result<Vec<RolloutRecord>>;

    /// 列出全部已持久化的会话,按时间倒序。开销低(只读每个文件的第一行)。
    async fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>>;

    /// 破坏性回退:丢弃 `to_turn_id`(含)及其之后的所有记录,只保留
    /// 出现在该 turn *之前* 的记录。`to_turn_id = None` 表示丢弃最后一轮
    /// (最近的 turn 边界)。
    ///
    /// 返回被丢弃的 `Message` 记录数(便于引擎在 `TurnRewoundEvent` 中
    /// 暴露 `truncated_after`)。当 turn id 不存在时返回 `0`(no-op)。
    /// 无法实现回退的实现(`NullRecorder`、桩、内存测试替身)返回 `Ok(0)`。
    ///
    /// 安全性:JSONL 实现会在截断前先写一个 `.bak` 兄弟文件,因此被丢弃
    /// 的 turn 仍可在磁盘上恢复。
    async fn truncate_after(&self, to_turn_id: Option<&TurnId>) -> anyhow::Result<usize>;
}

/// 空操作的 recorder。供不关心持久化的测试使用,也用于 `AgentConfig::default`,
/// 确保缺失 recorder 时不会 panic。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullRecorder;

#[async_trait]
impl RolloutRecorder for NullRecorder {
    async fn record(&self, _r: RolloutRecord) -> anyhow::Result<()> {
        Ok(())
    }

    async fn replay(&self, _session_id: ThreadId) -> anyhow::Result<Vec<RolloutRecord>> {
        Ok(Vec::new())
    }

    async fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        Ok(Vec::new())
    }

    async fn truncate_after(&self, _to_turn_id: Option<&TurnId>) -> anyhow::Result<usize> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_record_session_meta_serde() {
        let sid = ThreadId::new();
        let r = RolloutRecord::session_meta(sid, "openai/gpt-4o");
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"session_meta""#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_message_serde() {
        let tid = TurnId::new();
        let r = RolloutRecord::message(tid, MessageRole::User, serde_json::json!("hi"));
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"message""#));
        assert!(j.contains(r#""role":"user""#));
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_compaction_serde() {
        let tid = TurnId::new();
        let r = RolloutRecord::Compaction {
            turn_id: tid,
            strategy: "microcompact".into(),
            removed_count: 12,
            summary: "<summary>...</summary>".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"compaction""#));
        assert!(j.contains(r#""strategy":"microcompact""#));
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_fork_serde() {
        let parent = ThreadId::new();
        let r = RolloutRecord::Fork {
            parent_session_id: parent,
            branch_name: "explorer-branch".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"fork""#));
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_checkpoint_serde() {
        let tid = TurnId::new();
        let r = RolloutRecord::Checkpoint {
            turn_id: tid,
            sha: "abc123".into(),
            label: Some("before-refactor".into()),
            created_at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"checkpoint""#));
        assert!(j.contains(r#""sha":"abc123""#));
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_checkpoint_label_optional() {
        // label 缺省(None)必须能 round-trip(skip_serializing_if)。
        let j = r#"{"type":"checkpoint","turn_id":"00000000-0000-0000-0000-000000000001","sha":"def","created_at":"2026-07-02T00:00:00Z"}"#;
        let back: RolloutRecord = serde_json::from_str(j).unwrap();
        match back {
            RolloutRecord::Checkpoint { sha, label, .. } => {
                assert_eq!(sha, "def");
                assert_eq!(label, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_rewind_serde() {
        let tid = TurnId::new();
        let r = RolloutRecord::Rewind {
            turn_id: tid,
            target_sha: "abc123".into(),
            from_sha: "fff999".into(),
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"rewind""#));
        assert!(j.contains(r#""target_sha":"abc123""#));
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rollout_record_discussion_transcript_serde() {
        // M9: DiscussionTranscript 携带不透明 JSON payload,让 protocol crate
        // 与 reflect-discussion 的具体类型解耦。
        // v0.2.4: 同时携带可选 `agent_id` 用于 per-agent 切片;
        // None 分支必须通过 `skip_serializing_if` 完成 roundtrip。
        let did = Uuid::new_v4();
        let payload = serde_json::json!([
            {"id": 0, "from": "a", "kind": "utterance", "content": "hi"},
            {"id": 1, "from": "b", "kind": "consensus", "content": "agreed"},
        ]);
        let r = RolloutRecord::DiscussionTranscript {
            discussion_id: did,
            mode: "concurrent".into(),
            participants: vec!["a".into(), "b".into()],
            agent_id: None,
            transcript: payload.clone(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"discussion_transcript""#), "got: {j}");
        assert!(j.contains(r#""mode":"concurrent""#), "got: {j}");
        assert!(
            !j.contains("agent_id"),
            "None agent_id must be skipped via skip_serializing_if, got: {j}"
        );
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::DiscussionTranscript {
                discussion_id,
                mode,
                participants,
                agent_id,
                transcript,
            } => {
                assert_eq!(discussion_id, did);
                assert_eq!(mode, "concurrent");
                assert_eq!(participants, vec!["a", "b"]);
                assert_eq!(agent_id, None);
                assert_eq!(transcript, payload);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn discussion_transcript_with_agent_id_roundtrip() {
        // v0.2.4: per-agent transcript 切片。`Some(agent_id)` 必须能序列化
        // 并完成 round-trip。
        let did = Uuid::new_v4();
        let payload = serde_json::json!([
            {"id": 0, "from": "advocate", "kind": "utterance", "content": "I disagree"},
        ]);
        let r = RolloutRecord::DiscussionTranscript {
            discussion_id: did,
            mode: "sequential".into(),
            participants: vec!["advocate".into(), "skeptic".into()],
            agent_id: Some("advocate".into()),
            transcript: payload.clone(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""agent_id":"advocate""#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::DiscussionTranscript {
                discussion_id,
                agent_id,
                ..
            } => {
                assert_eq!(discussion_id, did);
                assert_eq!(agent_id.as_deref(), Some("advocate"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn discussion_transcript_backward_compat_with_m9_wire() {
        // M9 wire 形态:`{"type":"discussion_transcript", ...}` 不含 `agent_id`。
        // M9 producer 不会写入该字段;v0.2.4 consumer 必须将其解码为 `None`。
        let did = Uuid::new_v4();
        let j = format!(
            r#"{{"type":"discussion_transcript","discussion_id":"{did}","mode":"sequential","participants":["a"],"transcript":[]}}"#
        );
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::DiscussionTranscript {
                discussion_id,
                agent_id,
                ..
            } => {
                assert_eq!(discussion_id, did);
                assert_eq!(
                    agent_id, None,
                    "missing agent_id in M9 wire must decode as None"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn message_role_serde_uses_snake_case() {
        for (role, expected) in [
            (MessageRole::System, "\"system\""),
            (MessageRole::User, "\"user\""),
            (MessageRole::Assistant, "\"assistant\""),
            (MessageRole::Tool, "\"tool\""),
        ] {
            assert_eq!(serde_json::to_string(&role).unwrap(), expected);
        }
    }

    // ── v1.x:TokenCount 变体 serde ──────────────────────────────────

    #[test]
    fn rollout_record_token_count_serde() {
        let tid = TurnId::new();
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cached_tokens: 50,
            cache_write_tokens: 0,
            total_tokens: 1200,
        };
        let r = RolloutRecord::TokenCount {
            turn_id: tid,
            usage: usage.clone(),
            cost_usd: Some(0.0234),
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"token_count""#), "got: {j}");
        assert!(j.contains(r#""input_tokens":1000"#), "got: {j}");
        assert!(j.contains(r#""total_tokens":1200"#), "got: {j}");
        assert!(j.contains(r#""cost_usd":0.0234"#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::TokenCount {
                turn_id,
                usage: u,
                cost_usd,
                ..
            } => {
                assert_eq!(turn_id, tid);
                assert_eq!(u, usage);
                assert_eq!(cost_usd, Some(0.0234));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_token_count_cost_optional() {
        // cost_usd 缺省(None)必须能 round-trip(skip_serializing_if)。
        let j = r#"{"type":"token_count","turn_id":"00000000-0000-0000-0000-000000000002","usage":{"input_tokens":10,"output_tokens":5,"cached_tokens":0,"cache_write_tokens":0,"total_tokens":15},"at":"2026-07-31T00:00:00Z"}"#;
        let back: RolloutRecord = serde_json::from_str(j).unwrap();
        match back {
            RolloutRecord::TokenCount {
                usage, cost_usd, ..
            } => {
                assert_eq!(usage.total_tokens, 15);
                assert_eq!(cost_usd, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn session_info_token_fields_backward_compat() {
        // 旧 JSONL 不含 token 字段 → 反序列化应得到 0 / None。
        let sid = ThreadId::new();
        let old_json = format!(
            r#"{{"session_id":"{sid}","model":"m","started_at":"2026-07-14T00:00:00Z","message_count":3}}"#
        );
        let info: SessionInfo = serde_json::from_str(&old_json).unwrap();
        assert_eq!(info.session_id, sid);
        assert_eq!(info.input_tokens, 0, "missing field → default 0");
        assert_eq!(info.output_tokens, 0);
        assert_eq!(info.total_tokens, 0);
        assert_eq!(info.cost_usd, None, "missing cost_usd → None");
    }

    #[tokio::test]
    async fn null_recorder_is_noop() {
        let r = NullRecorder;
        let sid = ThreadId::new();
        r.record(RolloutRecord::session_meta(sid, "m"))
            .await
            .unwrap();
        assert!(r.replay(sid).await.unwrap().is_empty());
        assert!(r.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn null_recorder_truncate_after_is_zero() {
        // 批次二十二:truncate_after 在 NullRecorder 上是 no-op,返回 0。
        let r = NullRecorder;
        assert_eq!(r.truncate_after(None).await.unwrap(), 0);
        let tid = TurnId::new();
        assert_eq!(r.truncate_after(Some(&tid)).await.unwrap(), 0);
    }

    // ── v1.x:derive_title 单元测试 ──────────────────────────────────

    #[test]
    fn derive_title_short_text_kept_verbatim() {
        assert_eq!(
            derive_title("修复编译错误").as_deref(),
            Some("修复编译错误")
        );
    }

    #[test]
    fn derive_title_collapses_whitespace() {
        assert_eq!(
            derive_title("  修复\n\n编译\t错误  ").as_deref(),
            Some("修复 编译 错误")
        );
    }

    #[test]
    fn derive_title_long_text_truncated_with_ellipsis() {
        let long = "一".repeat(100);
        let t = derive_title(&long).expect("non-empty → Some");
        assert!(t.ends_with('…'));
        // 截到 TITLE_MAX_CHARS 个字符 + 1 个省略号。
        assert_eq!(t.chars().count(), TITLE_MAX_CHARS + 1);
    }

    #[test]
    fn derive_title_empty_returns_none() {
        assert_eq!(derive_title(""), None);
        assert_eq!(derive_title("   \n\t  "), None);
    }

    #[test]
    fn session_info_title_serde_backward_compat() {
        // 旧 JSONL 不含 title 字段 → 反序列化应得到 title = None。
        let sid = ThreadId::new();
        let old_json = format!(
            r#"{{"session_id":"{sid}","model":"m","started_at":"2026-07-14T00:00:00Z","message_count":3}}"#
        );
        let info: SessionInfo = serde_json::from_str(&old_json).unwrap();
        assert_eq!(info.session_id, sid);
        assert_eq!(info.title, None, "missing title field → None");
    }

    // ── v1.x:Plan / PermissionMode 变体 serde ────────────────────────

    #[test]
    fn rollout_record_plan_request_serde() {
        let pid = PlanId::new();
        let r = RolloutRecord::PlanRequest {
            plan_id: pid,
            task: "refactor auth module".into(),
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"plan_request""#), "got: {j}");
        assert!(j.contains(r#""task":"refactor auth module""#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::PlanRequest { plan_id, task, .. } => {
                assert_eq!(plan_id, pid);
                assert_eq!(task, "refactor auth module");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_plan_ready_with_path_serde() {
        let pid = PlanId::new();
        let r = RolloutRecord::PlanReady {
            plan_id: pid,
            markdown: "## Plan\n- step 1".into(),
            path: Some(PathBuf::from("/ws/.reflect/plan/abc.md")),
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"plan_ready""#), "got: {j}");
        assert!(
            j.contains(r#""path":"/ws/.reflect/plan/abc.md""#),
            "got: {j}"
        );
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::PlanReady {
                plan_id,
                markdown,
                path,
                ..
            } => {
                assert_eq!(plan_id, pid);
                assert_eq!(markdown, "## Plan\n- step 1");
                assert_eq!(
                    path.as_deref(),
                    Some(std::path::Path::new("/ws/.reflect/plan/abc.md"))
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_plan_ready_path_optional() {
        // path=None 必须 round-trip(skip_serializing_if)。
        // 用转义字符串而非 raw literal,避免 markdown 里的 `##` 与 `r#"..."#`
        // 终止符冲突(Edition 2024 reserved multi-hash token)。
        let j = "{\"type\":\"plan_ready\",\"plan_id\":\"00000000-0000-0000-0000-000000000003\",\"markdown\":\"## Plan\",\"at\":\"2026-08-05T00:00:00Z\"}";
        let back: RolloutRecord = serde_json::from_str(j).unwrap();
        match back {
            RolloutRecord::PlanReady { markdown, path, .. } => {
                assert_eq!(markdown, "## Plan");
                assert_eq!(path, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_plan_rejected_serde() {
        let pid = PlanId::new();
        let r = RolloutRecord::PlanRejected {
            plan_id: pid,
            reason: Some("user wants to revise".into()),
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""type":"plan_rejected""#), "got: {j}");
        assert!(j.contains(r#""reason":"user wants to revise""#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::PlanRejected {
                plan_id, reason, ..
            } => {
                assert_eq!(plan_id, pid);
                assert_eq!(reason.as_deref(), Some("user wants to revise"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_plan_rejected_reason_optional() {
        // reason=None 必须 round-trip(skip_serializing_if)。
        let j = r#"{"type":"plan_rejected","plan_id":"00000000-0000-0000-0000-000000000004","at":"2026-08-05T00:00:00Z"}"#;
        let back: RolloutRecord = serde_json::from_str(j).unwrap();
        match back {
            RolloutRecord::PlanRejected { reason, .. } => {
                assert_eq!(reason, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rollout_record_permission_mode_changed_serde() {
        let r = RolloutRecord::PermissionModeChanged {
            from: PermissionMode::Prompt,
            to: PermissionMode::Plan,
            at: Utc::now(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(
            j.contains(r#""type":"permission_mode_changed""#),
            "got: {j}"
        );
        assert!(j.contains(r#""from":"prompt""#), "got: {j}");
        assert!(j.contains(r#""to":"plan""#), "got: {j}");
        let back: RolloutRecord = serde_json::from_str(&j).unwrap();
        match back {
            RolloutRecord::PermissionModeChanged { from, to, .. } => {
                assert_eq!(from, PermissionMode::Prompt);
                assert_eq!(to, PermissionMode::Plan);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
