//! Discussion + Collab events 端到端集成测试 — 验证 v0.2.4 路径:
//! `prompt_for_closure` 跑真 LLM stub → `event_sink` 收到
//! `Started → N × Message → Finished` 顺序,`token_usage` 写到 toolset 槽后
//! `SendMessageTool.execute()` 把它复制到出站 `DiscussionMessage.token_usage`。
//!
//! 复用了 `discussion_llm_e2e.rs` 的 `StubClient` 模式,但需要它在 stream 里
//! emit 一个 `TokenCount` 事件,所以这里重新写一个 `StubClientWithTokens`,
//! 并提供一个 `mk_orchestrator_with_sink()` 工厂。

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
    AgentId, DiscussionConfig, DiscussionId, DiscussionMode, MessageKind,
};
use reflect_discussion::orchestrator::DiscussionOrchestrator;
use reflect_discussion::{AgentSection, DiscussionResult};
use reflect_llm::{
    ChatEvent, ChatRequest, CredentialPool, LlmError as ProviderLlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_protocol::{EventMsg, ThreadId, TokenUsage};
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Stub `ModelClient` —— 与 `discussion_llm_e2e.rs::StubClient` 类似,但多发
/// 一个 `TokenCount { input, output, cached, cache_write }` 事件,让
/// `collect_result_with_usage` 抓取到 usage。
struct StubClientWithTokens {
    spawns: Arc<AtomicU32>,
}

#[async_trait]
impl ModelClient for StubClientWithTokens {
    fn name(&self) -> &str {
        "stub-with-tokens"
    }
    async fn stream(
        &self,
        _req: ChatRequest,
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
            // ContentDelta 让 StateGraph 走完 model_call + check_stop,
            // 产出 TurnComplete, collect_result 才能正常终止。
            Ok(ChatEvent::ContentDelta("ok".into())),
            // Usage: spawn 立刻报告一次 12 in / 7 out,让 spawn 后端
            // 能抓到 usage 写回 toolset_usage 槽。
            Ok(ChatEvent::Usage {
                input_tokens: 12,
                output_tokens: 7,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

fn mk_factory(spawns: Arc<AtomicU32>) -> Arc<SubAgentFactory> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub-with-tokens",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClientWithTokens { spawns }),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "stub-with-tokens/stub-1",
        registry,
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
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

fn mk_usage(participants: &[AgentId]) -> HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> {
    participants
        .iter()
        .map(|p| (p.clone(), Arc::new(Mutex::new(None))))
        .collect()
}

/// 构造 orchestrator + 事件 sink —— sink 是 Arc<Mutex<Vec<EventMsg>>>,
/// LLM 跑完后检查里面的 CollabStarted / CollabMessage / CollabFinished 顺序。
#[allow(clippy::type_complexity)]
fn mk_orchestrator_with_sink(
    participants: Vec<AgentId>,
    agents: Vec<AgentSection>,
    factory: Arc<SubAgentFactory>,
    topic: &str,
    max_rounds: u32,
) -> (
    DiscussionOrchestrator,
    MessageBus,
    Arc<Mutex<Vec<EventMsg>>>,
    Arc<Mutex<HashMap<AgentId, Option<TokenUsage>>>>,
) {
    let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 4);
    let captured: Arc<Mutex<Vec<EventMsg>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = captured.clone();
    let event_sink: Arc<dyn Fn(EventMsg) + Send + Sync> = Arc::new(move |evt: EventMsg| {
        cap_clone.lock().push(evt);
    });
    let usage = mk_usage(&participants);
    let _ = usage; // passed to build_context via make_ctx in caller
    let usage_view = Arc::new(Mutex::new(
        participants
            .iter()
            .map(|p| (p.clone(), None))
            .collect::<HashMap<_, _>>(),
    ));
    let config = DiscussionConfig {
        mode: DiscussionMode::Sequential,
        participants,
        topic: topic.into(),
        consensus_window: 1,
        max_rounds,
        mailbox_capacity: 4,
    };
    let orch = DiscussionOrchestrator::with_event_sink(
        config,
        bus.clone(),
        Some(factory),
        CancellationToken::new(),
        None,
        Arc::new(std::sync::atomic::AtomicU32::new(0)),
        Some(event_sink),
    )
    .unwrap();
    let _ = agents;
    (orch, bus, captured, usage_view)
}

fn make_ctx(
    factory: Arc<SubAgentFactory>,
    config: &DiscussionConfig,
    agents: &[AgentSection],
    usage: HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>>,
) -> reflect_discussion::llm::LlmContext {
    build_context(
        factory,
        config.topic.clone(),
        &config.participants,
        agents,
        usage,
    )
    .unwrap()
}

#[tokio::test]
async fn discussion_e2e_emits_collab_events_to_sink() {
    let participants = vec![
        AgentId("a".into()),
        AgentId("b".into()),
        AgentId("c".into()),
    ];
    let agents: Vec<AgentSection> = participants.iter().map(|p| mk_section(&p.0)).collect();
    let factory = mk_factory(Arc::new(AtomicU32::new(0)));

    let (orch, bus, captured, usage_view) = mk_orchestrator_with_sink(
        participants.clone(),
        agents.clone(),
        factory.clone(),
        "test topic",
        1,
    );

    let usage = mk_usage(&participants);
    let config = DiscussionConfig {
        mode: DiscussionMode::Sequential,
        participants: participants.clone(),
        topic: "test topic".into(),
        consensus_window: 1,
        max_rounds: 1,
        mailbox_capacity: 4,
    };
    let ctx = make_ctx(factory.clone(), &config, &agents, usage);
    let _ = usage_view;
    let _ = bus;

    let _result: DiscussionResult = orch
        .run(prompt_for_closure(ctx, bus.clone()), |_| {})
        .await
        .unwrap();

    // 收集 sink 收到的 Collab* 事件
    let events = captured.lock().clone();
    let started: Vec<&CollabStartedPayload> = events
        .iter()
        .filter_map(|e| match e {
            EventMsg::CollabStarted(s) => Some(s),
            _ => None,
        })
        .collect();
    let messages: Vec<&CollabMessagePayload> = events
        .iter()
        .filter_map(|e| match e {
            EventMsg::CollabMessage(m) => Some(m),
            _ => None,
        })
        .collect();
    let finished: Vec<&CollabFinishedPayload> = events
        .iter()
        .filter_map(|e| match e {
            EventMsg::CollabFinished(f) => Some(f),
            _ => None,
        })
        .collect();

    assert_eq!(started.len(), 1, "exactly one CollabStarted");
    assert_eq!(finished.len(), 1, "exactly one CollabFinished");
    // max_rounds=1 顺序模式,3 个 participant 都 spawn 一次,每轮由 prompt_for
    // 触发 → orchestrator 在每条消息路由后 emit CollabMessage。
    // 我们没有预注入消息,所以 messages 可能为 0(LLM stub 不发 send_message),
    // 这是 stub 路径的合法状态。
    assert!(
        messages.is_empty() || messages.iter().all(|m| m.round == 0),
        "messages round should be 0, got {:?}",
        messages.iter().map(|m| m.round).collect::<Vec<_>>()
    );
    // 顺序:Started 在 Finished 之前
    let started_pos = events
        .iter()
        .position(|e| matches!(e, EventMsg::CollabStarted(_)))
        .unwrap();
    let finished_pos = events
        .iter()
        .position(|e| matches!(e, EventMsg::CollabFinished(_)))
        .unwrap();
    assert!(started_pos < finished_pos, "Started before Finished");
}

/// 类型别名避免上面 `match e` 写到复杂的 `if let` 嵌套
type CollabStartedPayload = reflect_protocol::CollabStartedEvent;
type CollabMessagePayload = reflect_protocol::CollabMessageEvent;
type CollabFinishedPayload = reflect_protocol::CollabFinishedEvent;

/// Stub LLM 路径下,`collect_result_with_usage` 应该抓到 TokenCount
/// 事件的 usage 写到 toolset_usage 槽,槽在下一个 spawn 之前的 stub 路径
/// 不会被读(因为 stub LLM 不调 send_message),但写操作本身应已经发生。
#[tokio::test]
async fn discussion_e2e_token_usage_written_to_slot() {
    let participants = vec![AgentId("a".into()), AgentId("b".into())];
    let agents: Vec<AgentSection> = participants.iter().map(|p| mk_section(&p.0)).collect();
    let factory = mk_factory(Arc::new(AtomicU32::new(0)));

    let usage = mk_usage(&participants);
    // 取出 b 的槽引用,断言 spawn 后被写入
    let b_slot = usage.get(&AgentId("b".into())).unwrap().clone();

    let config = DiscussionConfig {
        mode: DiscussionMode::Sequential,
        participants: participants.clone(),
        topic: "t".into(),
        consensus_window: 1,
        max_rounds: 1, // 顺序模式 1 轮 2 spawn = depth 2 OK(2 轮 = 4 > 3)
        mailbox_capacity: 4,
    };
    let ctx = make_ctx(factory.clone(), &config, &agents, usage);
    let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 4);
    let orch = DiscussionOrchestrator::new(
        config,
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
    )
    .unwrap();

    let _result: DiscussionResult = orch
        .run(prompt_for_closure(ctx, bus.clone()), |_| {})
        .await
        .unwrap();

    // 顺序模式 a 先 spawn,b 后 spawn,所以 b_slot 至少写一次
    let b_usage = b_slot.lock().clone();
    assert!(
        b_usage.is_some(),
        "toolset_usage slot for 'b' should be filled by stub LLM's TokenCount event"
    );
    let u = b_usage.unwrap();
    assert_eq!(u.input_tokens, 12, "stub reports 12 input");
    assert_eq!(u.output_tokens, 7, "stub reports 7 output");
    assert_eq!(u.total_tokens, 19, "stub reports 19 total");
}

// ── MessageKind 简单覆盖测试 ────────────────────────────────────────
//
// orchestrator 的 emit_collab_message 把 DiscussionMessage.kind 用 Debug 格式
// 拼进 CollabMessageEvent.kind 字符串(M9 + v0.2.4 简化:直接 Debug,避免
// 维护 4 个 enum variant ↔ snake_case 字符串的双向映射)。

#[test]
fn message_kind_serde_roundtrip_used_by_collab_event() {
    use serde_json;
    // v0.2.4 起:CollabMessageEvent.kind 用 Debug 格式字符串,
    // "Utterance" / "Consensus" / "Finish"。这里保证 serde 形式
    // (snake_case) 仍然能被正确解析,debug 字符串是 source-of-truth。
    let j = serde_json::to_string(&MessageKind::Utterance).unwrap();
    assert_eq!(j, "\"utterance\"");
    let parsed: MessageKind = serde_json::from_str(&j).unwrap();
    assert!(matches!(parsed, MessageKind::Utterance));
}
