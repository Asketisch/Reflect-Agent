//! 顶层 facade re-export 集成测试(v0.2.3)。
//!
//! 验证 `zhuye::*` 暴露的讨论 + LLM 集成 API 真的可用 —— 不只是 compile,
//! 还要能构造 `MessageBus` / `DiscussionConfig` / `DiscussionOrchestrator`
//! 等,然后跑一次 `run_noop` 走完整个 facade 表面。
//!
//! 真实 LLM 路径(`prompt_for_closure` + `SubAgentFactory`)留给 e2e 在
//! `zhuye-discussion/tests/discussion_llm_e2e.rs`;这里专注 "顶层
//! import 路径一切通"。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zhuye::{
    AgentId, AgentSection, DiscussionConfig, DiscussionId, DiscussionMode, DiscussionOrchestrator,
    MessageBus, OrchestratorEvent,
};
use zhuye_subagent::SubAgentFactory;

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
        judge: false,
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
        zhuye::DiscussionResult::NoConsensus { .. }
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

// ── v1.x:数据记录 SDK 化 —— telemetry / rollout 读取侧 + 统计 API ──────

#[test]
fn lib_facade_exposes_telemetry_read_api() {
    // 写入侧类型可达。
    let _sink_type: fn() -> Option<Arc<zhuye::TelemetrySink>> = || None;
    // 读取侧 API 可达(函数指针类型检查,不实际执行 I/O)。
    let _list_io: fn(&std::path::Path) -> Vec<(String, std::path::PathBuf, std::time::SystemTime)> =
        zhuye::list_model_io_files;
    let _read_io: fn(&std::path::Path) -> Vec<zhuye::ModelIoRecord> = zhuye::read_model_io_file;
    let _list_spans: fn(&std::path::Path, usize) -> Vec<(String, std::path::PathBuf)> =
        zhuye::list_span_log_files;
    let _read_spans: fn(&std::path::Path) -> Vec<zhuye::TraceEvent> = zhuye::read_span_log_file;
    let _resolve_dir: fn(Option<&std::path::Path>) -> std::path::PathBuf =
        zhuye::resolve_traces_dir;
    // 记录类型可达。
    let _usage = zhuye::UsageSnapshot {
        input_tokens: 1,
        output_tokens: 2,
        cached_tokens: 0,
        cache_write_tokens: 0,
        total_tokens: 3,
        cost_usd: None,
    };
    let _model_ref = zhuye::ModelRef {
        model_id: "openai/gpt-4o".into(),
        provider_id: Some("openai".into()),
        role: Some("main".into()),
        source: Some("main_turn".into()),
    };
}

#[test]
fn lib_facade_exposes_rollout_read_api_and_stats() {
    // rollout 读取侧 API 可达。
    let _list: fn(&std::path::Path) -> std::io::Result<Vec<zhuye::SessionInfo>> =
        zhuye::list_sessions;
    let _find: fn(&std::path::Path, zhuye::ThreadId) -> Option<std::path::PathBuf> =
        zhuye::find_session_path;
    let _markdown: fn(&[zhuye::RolloutRecord]) -> String = zhuye::to_markdown;
    // v1.x 统计 API 可达。
    let _session_stats: fn(&std::path::Path, zhuye::ThreadId) -> Option<zhuye::SessionStats> =
        zhuye::session_stats;
    let _all_models: fn(&std::path::Path) -> Vec<zhuye::ModelUsage> = zhuye::all_model_usage;
    // 统计类型可达且字段可访问。
    let usage = zhuye::ModelUsage {
        model: "openai/gpt-4o".into(),
        provider: None,
        calls: 1,
        input_tokens: 10,
        output_tokens: 5,
        cached_tokens: 0,
        cache_write_tokens: 0,
        total_tokens: 15,
        cost_usd: None,
    };
    assert_eq!(usage.total_tokens, 15);
}

#[test]
fn lib_facade_builder_exposes_telemetry_and_rollout_config() {
    // Builder 链式配置入口可达(不实际 build,避免依赖 API key)。
    let _b = zhuye::Zhuye::builder("openai/gpt-4o")
        .with_telemetry(None)
        .with_traces_dir("/tmp/zhuye-test-traces")
        .with_rollout_dir("/tmp/zhuye-test-sessions")
        .without_rollout();
}
