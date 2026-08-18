//! 顶层 facade re-export 集成测试(v0.2.3)。
//!
//! 验证 `reflect::*` 暴露的讨论 + LLM 集成 API 真的可用 —— 不只是 compile,
//! 还要能构造 `MessageBus` / `DiscussionConfig` / `DiscussionOrchestrator`
//! 等,然后跑一次 `run_noop` 走完整个 facade 表面。
//!
//! 真实 LLM 路径(`prompt_for_closure` + `SubAgentFactory`)留给 e2e 在
//! `reflect-discussion/tests/discussion_llm_e2e.rs`;这里专注 "顶层
//! import 路径一切通"。

use std::sync::Arc;

use reflect::{
    AgentId, AgentSection, DiscussionConfig, DiscussionId, DiscussionMode, DiscussionOrchestrator,
    MessageBus, OrchestratorEvent,
};
use reflect_subagent::SubAgentFactory;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn lib_facade_exposes_message_bus_and_orchestrator() {
    // 1. 顶层 `MessageBus` 可构造
    let participants = vec![AgentId("a".into()), AgentId("b".into())];
    let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 4);
    assert_eq!(bus.discussion_id(), bus.discussion_id());

    // 2. 顶层 `DiscussionConfig` 字段填充
    let config = DiscussionConfig {
        mode: DiscussionMode::Sequential,
        participants,
        topic: "facade smoke".into(),
        consensus_window: 1,
        max_rounds: 1,
        mailbox_capacity: 4,
    };
    assert_eq!(config.topic, "facade smoke");

    // 3. 顶层 `DiscussionOrchestrator` 可构造并跑通 (run_noop 状态机)
    let orch =
        DiscussionOrchestrator::new(config, bus.clone(), None, CancellationToken::new(), None)
            .expect("orchestrator should construct");

    let mut events = Vec::new();
    let result = orch
        .run_noop(|e| events.push(e))
        .await
        .expect("run_noop should succeed");
    // 2 agent × 1 round = 2 个 AgentTurn,加上 Started + Finished = 4 个事件。
    assert_eq!(events.len(), 4, "Started + 2 AgentTurn + Finished");
    assert!(matches!(events[0], OrchestratorEvent::Started { .. }));
    assert!(matches!(events[1], OrchestratorEvent::AgentTurn { .. }));
    assert!(matches!(events[2], OrchestratorEvent::AgentTurn { .. }));
    assert!(matches!(events[3], OrchestratorEvent::Finished { .. }));
    assert!(matches!(
        result,
        reflect::DiscussionResult::NoConsensus { .. }
    ));
}

#[test]
fn lib_facade_exposes_agent_section_and_misc_types() {
    // 顶层 `AgentSection` 可构造并访问字段
    let section = AgentSection {
        role: "advocate".into(),
        system_prompt: "argues for async".into(),
        allowed_tools: vec!["send_message".into()],
    };
    assert_eq!(section.role, "advocate");
    assert_eq!(section.allowed_tools.len(), 1);

    // 顶层 `SubAgentFactory` re-export 仍可用(放在 subagent crate 的那一组)
    let _factory_type_check: fn() -> Option<Arc<SubAgentFactory>> = || None;
}
