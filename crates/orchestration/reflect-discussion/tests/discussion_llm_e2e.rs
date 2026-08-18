//! Discussion LLM 端到端集成测试 — 验证 `prompt_for_closure` 接通
//! `SubAgentFactory::spawn` + drain `TurnHandle` 后,整套讨论状态机
//! 仍能正确运转。
//!
//! 策略:用一个会输出 1 个文本 token 的 stub `ModelClient`(模仿
//! `subagent_nested_depth.rs` 的 NoopClient 模式 + 加一个 ContentDelta 让
//! StateGraph 走到 `TurnComplete`),构造 3-agent discussion + factory,
//! 通过 `orch.run(prompt_for_closure(...), |_| {})` 跑完整循环。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use futures::Stream;
use futures::stream;
use parking_lot::Mutex;
use reflect_discussion::llm::{build_context, prompt_for_closure};
use reflect_discussion::message_bus::MessageBus;
use reflect_discussion::models::{
    AgentId, DiscussionConfig, DiscussionId, DiscussionMessage, DiscussionMode, MessageKind,
};
use reflect_discussion::orchestrator::DiscussionOrchestrator;
use reflect_discussion::{AgentSection, DiscussionResult};
use reflect_llm::{
    ChatEvent, ChatRequest, CredentialPool, LlmError as ProviderLlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_protocol::{ThreadId, TokenUsage};
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Stub `ModelClient`:每次 stream 返回 `MessageStart` + `ContentDelta("ok")` +
/// `MessageStop`。`ContentDelta` 让 StateGraph 走完 model_call 加 check_stop
/// 路径,产出 `TurnComplete`,`collect_result` 才能正常终止。
struct StubClient {
    spawns: Arc<AtomicU32>,
}

#[async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        "stub"
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ChatEvent, ProviderLlmError>> + Send>>,
        ProviderLlmError,
    > {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(vec![
            Ok(ChatEvent::MessageStart {
                id: "m".into(),
                model: "stub-1".into(),
            }),
            Ok(ChatEvent::ContentDelta("ok".into())),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

fn mk_factory(spawns: Arc<AtomicU32>) -> Arc<SubAgentFactory> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient { spawns }),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cancel = CancellationToken::new();
    let tools = Arc::new(ToolRegistry::default());
    Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "stub/stub-1",
        registry,
        None, // child_registry: 回退父级 registry
        tools,
        cancel,
        None,
    ))
}

fn mk_section(role: &str) -> AgentSection {
    AgentSection {
        role: role.into(),
        system_prompt: format!("you are {role}"),
        allowed_tools: vec![
            "send_message".into(),
            "read_messages".into(),
            "finish_discussion".into(),
        ],
    }
}

/// v0.2.4:为每个 participant 构造一个空 `Arc<Mutex<Option<TokenUsage>>>` 槽,
/// 喂给 `build_context` 的最后一个参数。stub 路径不写真实 usage,但槽必须存在
/// 以满足新签名校验。
fn mk_usage(participants: &[AgentId]) -> HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> {
    participants
        .iter()
        .map(|p| (p.clone(), Arc::new(Mutex::new(None))))
        .collect()
}

fn mk_config(mode: DiscussionMode, max_rounds: u32) -> (DiscussionConfig, Vec<AgentSection>) {
    let participants = vec![
        AgentId("a".into()),
        AgentId("b".into()),
        AgentId("c".into()),
    ];
    let agents: Vec<AgentSection> = participants.iter().map(|p| mk_section(&p.0)).collect();
    let config = DiscussionConfig {
        mode,
        participants,
        topic: "test topic".into(),
        consensus_window: 1,
        max_rounds,
        mailbox_capacity: 8,
    };
    (config, agents)
}

/// Sequential / 1 round:3 个 stub agent 各 spawn 一次,断言 depth 推进到 3。
#[tokio::test]
async fn discussion_llm_e2e_sequential_three_agents_depth_advances() {
    let spawns = Arc::new(AtomicU32::new(0));
    let factory = mk_factory(spawns.clone());
    let (config, agents) = mk_config(DiscussionMode::Sequential, 1);
    let bus = MessageBus::new(
        DiscussionId::new(),
        config.participants.clone(),
        config.mailbox_capacity,
    );
    let ctx = build_context(
        factory.clone(),
        config.topic.clone(),
        &config.participants,
        &agents,
        mk_usage(&config.participants),
    )
    .unwrap();
    let orch = DiscussionOrchestrator::new(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
    )
    .unwrap();

    let result = orch
        .run(prompt_for_closure(ctx, bus.clone()), |_| {})
        .await
        .unwrap();

    assert!(
        matches!(result, DiscussionResult::NoConsensus { .. }),
        "got: {result:?}"
    );
    assert_eq!(factory.depth(), 3, "sequential spawn 3 times, depth = 3");
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        3,
        "ModelClient invoked 3 times"
    );
}

/// Concurrent / 1 round:3 个 stub agent 并发 spawn,断言 depth 推进到 3
/// (并发模式下每个 spawn 调用 depth += 1,且不互等)。
#[tokio::test]
async fn discussion_llm_e2e_concurrent_three_agents_depth_advances() {
    let spawns = Arc::new(AtomicU32::new(0));
    let factory = mk_factory(spawns.clone());
    let (config, agents) = mk_config(DiscussionMode::Concurrent, 1);
    let bus = MessageBus::new(
        DiscussionId::new(),
        config.participants.clone(),
        config.mailbox_capacity,
    );
    let ctx = build_context(
        factory.clone(),
        config.topic.clone(),
        &config.participants,
        &agents,
        mk_usage(&config.participants),
    )
    .unwrap();
    let orch = DiscussionOrchestrator::new(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
    )
    .unwrap();

    let result = orch
        .run(prompt_for_closure(ctx, bus.clone()), |_| {})
        .await
        .unwrap();

    assert!(
        matches!(result, DiscussionResult::NoConsensus { .. }),
        "got: {result:?}"
    );
    assert_eq!(factory.depth(), 3, "concurrent 3-agent round → depth = 3");
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        3,
        "ModelClient invoked 3 times"
    );
}

/// Sequential / 1 round,预先往 bus 注入 3 条 `MessageKind::Consensus` ——
///
/// 期望:`prompt_for_closure` 跑完后 `check_consensus` 命中已注入消息,
/// 讨论返回 `DiscussionResult::Consensus`。验证 LLM 路径与既有共识
/// 检测逻辑兼容(consensus 由 bus 既有消息触发,不由 stub LLM 触发)。
#[tokio::test]
async fn discussion_llm_e2e_sequential_with_preinjected_consensus() {
    let spawns = Arc::new(AtomicU32::new(0));
    let factory = mk_factory(spawns.clone());
    let (config, agents) = mk_config(DiscussionMode::Sequential, 1);
    let bus = MessageBus::new(
        DiscussionId::new(),
        config.participants.clone(),
        config.mailbox_capacity,
    );
    // 预先注入 3 条 Consensus(round=0)
    for a in &["a", "b", "c"] {
        bus.route(DiscussionMessage {
            id: Default::default(),
            discussion_id: bus.discussion_id(),
            from: AgentId((*a).into()),
            kind: MessageKind::Consensus,
            content: format!("{a} agrees"),
            recipients: vec![],
            round: 0,
            token_usage: Default::default(),
        })
        .await
        .unwrap();
    }
    let ctx = build_context(
        factory.clone(),
        config.topic.clone(),
        &config.participants,
        &agents,
        mk_usage(&config.participants),
    )
    .unwrap();
    let orch = DiscussionOrchestrator::new(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
    )
    .unwrap();

    // Sequential 第 1 个 agent 跑完 → check_consensus 命中 → 立即返回 Consensus
    // (不会跑完 3 个 spawn,depth 只到 1)
    let result = orch
        .run(prompt_for_closure(ctx, bus.clone()), |_| {})
        .await
        .unwrap();

    assert!(
        matches!(result, DiscussionResult::Consensus { final_round: 0, .. }),
        "expected Consensus at round 0, got: {result:?}"
    );
    assert!(
        factory.depth() <= 3,
        "depth should advance at most 3 times, got: {}",
        factory.depth()
    );
    assert!(
        spawns.load(Ordering::SeqCst) <= 3,
        "ModelClient invoked at most 3 times, got: {}",
        spawns.load(Ordering::SeqCst)
    );
}

/// Sequential / 3 rounds × 3 agents = 9 spawns(超 MAX_DEPTH=3)—— 验证
/// Sequential 模式下 depth 在跑第 2 轮时已经超上限,后续 spawn 返回
/// `MaxDepthExceeded`,闭包将其转 `RuntimeError::PromptBuilder` 后由
/// orchestrator 透传为 `Err(OrchestratorError::Runtime(...))`。
///
/// 这是 v0.2.x 已知限制的 live demo,留作 v0.3.x 修复(depth 在
/// `collect_result` 末尾自减)的回归测试基线。
#[tokio::test]
async fn discussion_llm_e2e_sequential_depth_exhausted_after_three_rounds() {
    let spawns = Arc::new(AtomicU32::new(0));
    let factory = mk_factory(spawns.clone());
    let (config, agents) = mk_config(DiscussionMode::Sequential, 3);
    let bus = MessageBus::new(
        DiscussionId::new(),
        config.participants.clone(),
        config.mailbox_capacity,
    );
    let ctx = build_context(
        factory.clone(),
        config.topic.clone(),
        &config.participants,
        &agents,
        mk_usage(&config.participants),
    )
    .unwrap();
    let orch = DiscussionOrchestrator::new(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
    )
    .unwrap();

    let result = orch.run(prompt_for_closure(ctx, bus.clone()), |_| {}).await;

    // Sequential 第 1 轮 3 spawn 成功 → 第 2 轮第 1 个 spawn 失败(MaxDepthExceeded)
    // → RuntimeError::PromptBuilder("spawn failed: ...") → OrchestratorError::Runtime
    let err = result.expect_err("expected runtime error after depth exhausted");
    let err_str = format!("{err:?}");
    assert!(
        err_str.contains("PromptBuilder") || err_str.contains("MaxDepthExceeded"),
        "expected depth-exhausted error, got: {err_str}"
    );
    assert_eq!(
        factory.depth(),
        3,
        "depth stuck at MAX_DEPTH after exhaustion"
    );
}
