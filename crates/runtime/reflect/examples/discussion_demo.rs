//! `discussion_demo` — 跑一个 3-agent 多 Agent 讨论(Reflect v0.2.x)。
//!
//! v0.2.x 起:`prompt_for_closure` 接通真实 LLM wiring。这里用一个 stub
//! `ModelClient`(返回 `MessageStart + ContentDelta + MessageStop`)代替
//! 真实 provider,这样 example **offline-friendly**(无需 API key),同时
//! 演示完整 wiring 流程:构造 `SubAgentFactory` → 构建 `LlmContext` →
//! `orch.run(prompt_for_closure(...), |e| println!(...))`。
//!
//! 真实 LLM 流程由 `reflect discussion run -c discussion.toml` CLI 接管
//! (见 `crates/reflect-discussion/examples/discussion.toml` + `cli::try_build_llm_orchestrator`)。
//!
//! Run:
//! ```bash
//! cargo run -p reflect --example discussion_demo
//! ```

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use futures::Stream;
use futures::stream;
use parking_lot::Mutex;
// v0.2.3 起:全部从顶层 `reflect::*` 拿,演示高级 API 也走 facade。
use reflect::{
    AgentId, AgentSection, DiscussionConfig, DiscussionId, DiscussionMode, DiscussionOrchestrator,
    DiscussionResult, MessageBus, OrchestratorEvent, build_context, prompt_for_closure,
};
use reflect_discussion::tool::DiscussionToolSet;
use reflect_llm::{
    ChatEvent, ChatRequest, CredentialPool, LlmError as ProviderLlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_protocol::{ThreadId, TokenUsage};
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Stub `ModelClient` —— 返回 `MessageStart` + 1 token `ContentDelta` +
/// `MessageStop`。`ContentDelta` 让 StateGraph 走完 `model_call → check_stop`
/// 路径,产出 `TurnComplete`,`collect_result` 能正常 drain。
struct DemoStubClient {
    spawns: Arc<AtomicU32>,
}

#[async_trait]
impl ModelClient for DemoStubClient {
    fn name(&self) -> &str {
        "demo-stub"
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
                model: "demo-stub-1".into(),
            }),
            Ok(ChatEvent::ContentDelta("(stub reply)".into())),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 构造 3 个 agent(advocate / skeptic / moderator)
    let participants = vec![
        AgentId("advocate".into()),
        AgentId("skeptic".into()),
        AgentId("moderator".into()),
    ];

    // 2. 构造 DiscussionConfig
    let config = DiscussionConfig {
        mode: DiscussionMode::Sequential, // Sequential 避开 Concurrent depth 限制
        participants: participants.clone(),
        topic: "Decide whether to use Rust async or sync for the new CLI parser".into(),
        consensus_window: 1,
        max_rounds: 1, // 1 round × 3 agents = 3 spawn,正好命中 MAX_DEPTH
        mailbox_capacity: 32,
    };

    // 3. 构造共享 MessageBus
    let bus = MessageBus::new(
        DiscussionId::new(),
        participants.clone(),
        config.mailbox_capacity,
    );

    // 4. 构造 stub ModelClient + SubAgentFactory(offline-friendly,无需 API key)
    let spawns = Arc::new(AtomicU32::new(0));
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "demo-stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(DemoStubClient {
                    spawns: spawns.clone(),
                }),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let finished = Arc::new(Mutex::new(false));
    // v0.2.3 起:DiscussionToolSet 新增 round 字段,所有 participant 共享同一个
    // AtomicU32,runtime 在每轮 store(round),comm_tools execute 时 load。
    let round_counter = Arc::new(AtomicU32::new(0));
    // v0.2.4 起:per-agent token usage 槽,与 DiscussionToolSet 和 LlmContext
    // 共享(空槽,stub 路径不写入)。
    let toolset_usage: HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> = participants
        .iter()
        .map(|p| (p.clone(), Arc::new(Mutex::new(None))))
        .collect();
    let parent_tools = Arc::new(ToolRegistry::default());
    for p in &participants {
        let slot = toolset_usage
            .get(p)
            .cloned()
            .expect("toolset_usage covers all participants");
        let set = DiscussionToolSet::with_usage(
            p.clone(),
            bus.clone(),
            finished.clone(),
            round_counter.clone(),
            slot,
        );
        set.verify()
            .expect("DiscussionToolSet should always verify");
        for name in set.tool_names() {
            if let Some(t) = set.registry.get(&name) {
                parent_tools.register(t);
            }
        }
    }
    let factory = Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "demo-stub/demo-stub-1",
        registry,
        None, // child_registry: 回退父级 registry
        parent_tools,
        CancellationToken::new(),
        None,
    ));

    // 5. 构造 orchestrator(v0.2.3:共享同一 round_counter 给 orchestrator,
    // 让 DiscussionRuntime 每轮 store(round))
    let orch = DiscussionOrchestrator::with_round_counter(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
        round_counter.clone(),
    )?;

    // 6. 构造 LlmContext + prompt_for_closure
    let agents: Vec<AgentSection> = participants
        .iter()
        .map(|p| AgentSection {
            role: p.0.clone(),
            system_prompt: format!("You are the {p} of the discussion.", p = p.0),
            allowed_tools: vec![
                "send_message".into(),
                "read_messages".into(),
                "finish_discussion".into(),
            ],
        })
        .collect();
    let ctx = build_context(
        factory.clone(),
        config.topic.clone(),
        &config.participants,
        &agents,
        toolset_usage,
    )?;
    let prompt_for = prompt_for_closure(ctx, bus.clone());

    // 7. 跑(真 wiring:每次 prompt_for 调用 → factory.spawn + drain TurnHandle)
    println!(
        "[discussion_demo] starting discussion (mode = sequential, max_rounds = 1, wiring = real)"
    );
    let result = orch
        .run(prompt_for, |event| match event {
            OrchestratorEvent::Started {
                id,
                participants,
                mode,
            } => {
                println!(
                    "[discussion_demo] started: id={} participants={:?} mode={:?}",
                    id, participants, mode
                );
            }
            OrchestratorEvent::AgentTurn { agent, round } => {
                println!(
                    "[discussion_demo] agent_turn: agent={} round={}",
                    agent.0, round
                );
            }
            OrchestratorEvent::Finished { result } => {
                println!("[discussion_demo] finished: {:?}", result);
            }
        })
        .await?;

    // 8. 输出 transcript
    let transcript = bus.format_transcript();
    println!("\n=== Transcript ===\n{transcript}");

    // 9. 输出 wiring stats
    println!(
        "[discussion_demo] factory.depth() = {} (sequential 1-round × 3-agent = 3 spawns)",
        factory.depth()
    );
    println!(
        "[discussion_demo] ModelClient invoked {} times",
        spawns.load(Ordering::SeqCst)
    );

    // 10. 退出
    match result {
        DiscussionResult::Consensus {
            final_round,
            summary,
        } => {
            println!("[discussion_demo] consensus at round {final_round}: {summary}");
        }
        DiscussionResult::NoConsensus {
            rounds_completed,
            transcript_len,
        } => {
            println!(
                "[discussion_demo] no consensus after {rounds_completed} rounds ({transcript_len} messages)"
            );
        }
        DiscussionResult::Finished { by, final_round } => {
            println!("[discussion_demo] finished by {by} at round {final_round}");
        }
    }

    Ok(())
}
