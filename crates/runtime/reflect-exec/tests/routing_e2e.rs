//! v1.0 多 Provider 路由:端到端 1-turn failover 测试。
//!
//! 跑 1 个 turn,主 credential 故意 429(429 + retry_after_ms = 1s),
//! 断言 turn 走完 + `StreamError.tried` 列表 + `RoutingEvent(Switched)` +
//! `TokenCountEvent` 带 fallback credential 的 label。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, RoutingEventKind, Submission};
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

struct FlakyClient {
    call_count: Arc<Mutex<u32>>,
    first_error: Option<LlmError>,
}
#[async_trait]
impl ModelClient for FlakyClient {
    fn name(&self) -> &str {
        "flaky"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let mut n = self.call_count.lock();
        *n += 1;
        if *n == 1
            && let Some(err) = self.first_error.clone()
        {
            return Err(err);
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(ChatEvent::ContentDelta("ok".into())),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

fn make_sub(text: &str) -> Submission {
    Submission::user_input(text)
}

#[tokio::test]
async fn main_turn_fails_over_and_completes() {
    let registry = Arc::new(ModelRegistry::new());

    let count_a = Arc::new(Mutex::new(0u32));
    let count_b = Arc::new(Mutex::new(0u32));
    let count_a_c = count_a.clone();
    let count_b_c = count_b.clone();

    let c_a: Arc<dyn ModelClient> = Arc::new(FlakyClient {
        call_count: count_a_c,
        first_error: Some(LlmError::RateLimited { retry_after_ms: 1 }),
    });
    let c_b: Arc<dyn ModelClient> = Arc::new(FlakyClient {
        call_count: count_b_c,
        first_error: None,
    });
    registry.register_pool(
        "openai",
        CredentialPool {
            entries: vec![
                PoolEntry {
                    client: c_a,
                    label: "work".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c_b,
                    label: "personal".into(),
                    weight: 1,
                },
            ],
        },
    );

    // RoutingPolicy: main role 用 openai/gpt-4o,fallback 默认空
    let policy = Arc::new(reflect_llm::RoutingPolicy {
        main: reflect_llm::SpecSlot::with_primary("openai/gpt-4o".to_string()),
        ..Default::default()
    });

    let cfg = AgentConfig::new("openai/gpt-4o", Path::new("."))
        .with_policy(policy)
        .with_initial_permission_mode(reflect_protocol::PermissionMode::Auto);
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);
    let mut handle = thread.submit(make_sub("hi")).await;

    let mut saw_stream_error = false;
    let mut saw_routing_switched = false;
    let mut saw_token_count = false;
    let mut saw_turn_complete = false;
    let mut token_count_provider: Option<String> = None;
    let mut token_count_label: Option<String> = None;

    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::StreamError(ref s) if s.code == "RATE_LIMITED" => {
                saw_stream_error = true;
            }
            EventMsg::Routing(ref r)
                if matches!(r.kind, RoutingEventKind::Switched) && r.role == "main" =>
            {
                saw_routing_switched = true;
            }
            EventMsg::TokenCount(ref t) if t.provider.is_some() => {
                // 仅记录 model_call 阶段带 provider/credential 的 TokenCount。
                // submission_loop 在 turn 末会再发一次仅含累计 usage 的 TokenCount,
                // 那里 provider/credential 字段为 None,与本测试无关。
                saw_token_count = true;
                token_count_provider = t.provider.clone();
                token_count_label = t.credential_label.clone();
            }
            EventMsg::TurnComplete(_) => saw_turn_complete = true,
            _ => {}
        }
    }

    assert!(saw_stream_error, "expected StreamError(RATE_LIMITED)");
    assert!(
        saw_routing_switched,
        "expected RoutingEvent(Switched) on main failover"
    );
    assert!(saw_turn_complete, "expected successful TurnComplete");
    assert!(saw_token_count, "expected TokenCount event");
    assert_eq!(
        token_count_provider.as_deref(),
        Some("flaky"),
        "TokenCount 应报告 fallback credential 的 provider"
    );
    assert_eq!(
        token_count_label.as_deref(),
        Some("personal"),
        "TokenCount 应报告 fallback credential 的 label"
    );
    assert_eq!(*count_a.lock(), 1, "work credential called once");
    assert_eq!(
        *count_b.lock(),
        1,
        "personal credential called once after failover"
    );
}

/// 回归:`RoutingPolicy.main.primary` 是 `"provider/model"` 形式 spec,
/// 但发给上游 API 的 `ChatRequest.model` 必须是纯模型名(无 `provider/`
/// 前缀)。否则上游会因模型名含前缀而返回 404 model_invalid(曾出现在
/// StepFun 等第三方 Anthropic 兼容端点上)。
///
/// 见 `reflect-core/src/graph/nodes/mod.rs` 中 `ChatRequest { model: ... }`
/// 构造处的 `spec.split_once('/')` 剥前缀逻辑。
struct RecordingClient {
    last_request: Arc<Mutex<Option<ChatRequest>>>,
}
#[async_trait]
impl ModelClient for RecordingClient {
    fn name(&self) -> &str {
        "recording"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        *self.last_request.lock() = Some(request);
        Ok(Box::pin(stream::iter(vec![
            Ok(ChatEvent::MessageStart {
                id: "m1".into(),
                model: "step-3.7-flash".into(),
            }),
            Ok(ChatEvent::ContentDelta("ok".into())),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

#[tokio::test]
async fn chat_request_model_strips_provider_prefix() {
    let registry = Arc::new(ModelRegistry::new());
    let last_request: Arc<Mutex<Option<ChatRequest>>> = Arc::new(Mutex::new(None));
    let client: Arc<dyn ModelClient> = Arc::new(RecordingClient {
        last_request: last_request.clone(),
    });
    registry.register_pool(
        "anthropic",
        CredentialPool {
            entries: vec![PoolEntry {
                client,
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    // primary 用带前缀的 spec;断言剥前缀后 ChatRequest.model == "step-3.7-flash"
    let policy = Arc::new(reflect_llm::RoutingPolicy {
        main: reflect_llm::SpecSlot::with_primary("anthropic/step-3.7-flash".to_string()),
        ..Default::default()
    });
    let cfg = AgentConfig::new("anthropic/step-3.7-flash", Path::new("."))
        .with_policy(policy)
        .with_initial_permission_mode(reflect_protocol::PermissionMode::Auto);
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);
    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = last_request
        .lock()
        .clone()
        .expect("model_call should have hit the recording client");
    assert_eq!(
        req.model, "step-3.7-flash",
        "ChatRequest.model 必须剥掉 provider 前缀,实际: {}",
        req.model
    );
}

/// v1.1.0 Phase 4:coordinator 模式下 worker registry 排除 internal tools。
#[test]
fn coordinator_worker_registry_strips_internal_tools() {
    use reflect_task::coordinator::{INTERNAL_WORKER_TOOLS, build_worker_tool_registry};
    use reflect_tools::ToolRegistry;

    struct Stub(&'static str);
    #[async_trait::async_trait]
    impl reflect_tools::Tool for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _: reflect_tools::ToolContext,
            _: serde_json::Value,
        ) -> Result<reflect_protocol::ToolOutput, reflect_tools::ToolError> {
            unimplemented!()
        }
    }

    let parent = ToolRegistry::default();
    for n in INTERNAL_WORKER_TOOLS {
        parent.register(Arc::new(Stub(n)));
    }
    parent.register(Arc::new(Stub("TaskCreate")));

    let worker = build_worker_tool_registry(&parent);
    for forbidden in INTERNAL_WORKER_TOOLS {
        assert!(!worker.list().contains(&forbidden.to_string()));
    }
    assert!(worker.list().contains(&"TaskCreate".to_string()));
}
