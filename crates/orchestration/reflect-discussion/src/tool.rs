//! `tool` — 把 [`comm_tools`] 三个工具包装成 `ToolSource::Builtin`,注册到
//! [`ToolRegistry`](reflect_tools::ToolRegistry)。
//!
//! v0 入口是 [`DiscussionToolSet::new`],它一次性给单个 agent 构造:
//! - `send_message` / `read_messages` / `finish_discussion` 三个 Tool(均 `Builtin` source)
//! - 一个 `Arc<ToolRegistry>` 持有上述工具
//! - 共享 bus + `finished` flag(让 agent 的工具调用能直接读到讨论状态)
//!
//! 调用方把 `DiscussionToolSet.registry` 与 subagent 工厂的 `child_tools`
//! 合并:subagent 自身的 `allowed_tools` 决定哪些工具被 `factory.spawn` 透传
//! 给子 `AgentThread`,三个 comm_tools 必须出现在 `allowed_tools` 里,否则
//! LLM 看不到这些工具。

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use parking_lot::Mutex;
use reflect_protocol::{TokenUsage, ToolError};
use reflect_tools::ToolRegistry;

use crate::comm_tools::{FinishDiscussionTool, ReadMessagesTool, SendMessageTool};
use crate::message_bus::MessageBus;
use crate::models::AgentId;

/// 一个 agent 视角的完整讨论工具集合。
///
/// `Clone` 让多 agent 场景下每个 agent 拿到独立的 `ToolRegistry` + 独立 bus
/// 视图(但 bus 内部是 `Arc<Inner>`,共享 transcript / next_id;`finished`
/// flag 也通过 `Arc<Mutex<>>>` 共享),避免状态泄漏。
///
/// v0.2.3 起:`round` 字段是 `Arc<AtomicU32>`,由 [`crate::runtime::DiscussionRuntime`]
/// 在每轮开始前 `store(round, SeqCst)`,comm_tools 在 `execute()` 读出当前
/// 轮次后写入 `DiscussionMessage.round`。所有 participant 共享同一个
/// `Arc<AtomicU32>`,所以 `DiscussionToolSet::new` 一次构造后所有 agent
/// 同步推进。
///
/// v0.2.4 起:`token_usage` 是 `Arc<Mutex<Option<TokenUsage>>>`,由
/// `prompt_for_closure` 在 `SpawnedChild::collect_result_with_usage().await`
/// 之后写入;`SendMessageTool.execute()` 把该 slot 快照翻译到出站
/// `DiscussionMessage.token_usage`(`BTreeMap` 形式)。non-LLM / 早终止路径下
/// slot 始终 `None`,消息 token_usage 保持空 map。
#[derive(Clone)]
pub struct DiscussionToolSet {
    /// 当前 agent 标识(注入到三个 comm_tools 的 `self_id`)。
    pub self_id: AgentId,
    /// 共享讨论 bus(`MessageBus` 自身 `Clone`)。
    pub bus: MessageBus,
    /// 共享"已结束"标志(防止 `finish_discussion` 被多次调用)。
    pub finished: Arc<Mutex<bool>>,
    /// 共享轮次计数器,由 runtime 每轮 `store(round)`,comm_tools 读取后
    /// 写入 `DiscussionMessage.round`(v0.2.3 起)。
    pub round: Arc<AtomicU32>,
    /// v0.2.4: 共享 token usage 槽(spawning thread 写、`SendMessageTool`
    /// 读)。`None` 表示本 turn 还没 LLM 注入 usage(早终止 / non-LLM 路径)。
    pub token_usage: Arc<Mutex<Option<TokenUsage>>>,
    /// 持有三个 comm_tools 的 `ToolRegistry`。
    pub registry: Arc<ToolRegistry>,
}

impl DiscussionToolSet {
    /// 给一个 agent 构造讨论工具集合(注册三个 comm_tools 到 `Builtin` 源)。
    ///
    /// `round` 由 `DiscussionRuntime` 持有并定期刷新;多个 participant 共享
    /// 同一个 `Arc<AtomicU32>` 才能在同轮内同步前进。CLI / 程序化构造者
    /// 用 `Arc::new(AtomicU32::new(0))` 起一个,再 clone 给每个 toolset +
    /// orchestrator。
    ///
    /// v0.2.4:`token_usage` slot 默认是 `Arc::new(Mutex::new(None))`(空槽)。
    /// 实战 LLM 路径下由 `try_build_llm_orchestrator` 注入一个共享的
    /// `Arc<Mutex<Option<TokenUsage>>>`(per agent 一个),spawning thread
    /// 在 `collect_result_with_usage` 后写入。
    pub fn new(
        self_id: AgentId,
        bus: MessageBus,
        finished: Arc<Mutex<bool>>,
        round: Arc<AtomicU32>,
    ) -> Self {
        Self::with_usage(self_id, bus, finished, round, Arc::new(Mutex::new(None)))
    }

    /// v0.2.4: 显式传入 token usage 槽。`try_build_llm_orchestrator` 在拼装
    /// 每个 agent 的 toolset 时调用这个版本,把 orchestrator 维护的 per-agent
    /// usage map 共享出来,让 `SendMessageTool.execute()` 能读到 spawning
    /// thread 写入的 usage。
    pub fn with_usage(
        self_id: AgentId,
        bus: MessageBus,
        finished: Arc<Mutex<bool>>,
        round: Arc<AtomicU32>,
        token_usage: Arc<Mutex<Option<TokenUsage>>>,
    ) -> Self {
        let registry = Arc::new(ToolRegistry::default());
        registry.register(Arc::new(SendMessageTool {
            self_id: self_id.clone(),
            bus: bus.clone(),
            round: round.clone(),
            token_usage: token_usage.clone(),
        }));
        registry.register(Arc::new(ReadMessagesTool {
            self_id: self_id.clone(),
            bus: bus.clone(),
        }));
        registry.register(Arc::new(FinishDiscussionTool {
            self_id: self_id.clone(),
            bus: bus.clone(),
            finished: finished.clone(),
            round: round.clone(),
        }));
        Self {
            self_id,
            bus,
            finished,
            round,
            token_usage,
            registry,
        }
    }

    /// 校验三个 comm_tools 都已注册(失败时返回 [`ToolError::Execution`])。
    pub fn verify(&self) -> Result<(), ToolError> {
        for name in ["send_message", "read_messages", "finish_discussion"] {
            if self.registry.get(name).is_none() {
                return Err(ToolError::Execution(format!(
                    "missing discussion tool '{name}'"
                )));
            }
        }
        Ok(())
    }

    /// 列出本集合里所有工具名(按字典序;用于调试或拼装 `SubAgentSpec::allowed_tools`)。
    pub fn tool_names(&self) -> Vec<String> {
        self.registry.list()
    }
}

impl std::fmt::Debug for DiscussionToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionToolSet")
            .field("self_id", &self.self_id)
            .field("tools", &self.tool_names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use crate::models::DiscussionId;

    fn mk_set(agent: AgentId) -> DiscussionToolSet {
        let bus = MessageBus::new(
            DiscussionId::new(),
            vec![agent.clone(), AgentId("other".into())],
            4,
        );
        let finished = Arc::new(Mutex::new(false));
        DiscussionToolSet::new(agent, bus, finished, Arc::new(AtomicU32::new(0)))
    }

    #[test]
    fn new_registers_all_three_comm_tools() {
        let set = mk_set(AgentId("a".into()));
        assert!(set.registry.get("send_message").is_some());
        assert!(set.registry.get("read_messages").is_some());
        assert!(set.registry.get("finish_discussion").is_some());
    }

    #[test]
    fn verify_passes_when_all_tools_registered() {
        let set = mk_set(AgentId("a".into()));
        assert!(set.verify().is_ok());
    }

    #[test]
    fn tool_names_returns_sorted_three() {
        let set = mk_set(AgentId("a".into()));
        assert_eq!(
            set.tool_names(),
            vec![
                "finish_discussion".to_string(),
                "read_messages".to_string(),
                "send_message".to_string(),
            ],
        );
    }

    #[test]
    fn discussion_tool_set_is_cloneable() {
        // 多个 agent 场景需要 clone(共享 bus inner,独立 registry)
        let set = mk_set(AgentId("a".into()));
        let set2 = set.clone();
        // 两个 registry 都拿到 send_message
        assert!(set2.registry.get("send_message").is_some());
    }
}
