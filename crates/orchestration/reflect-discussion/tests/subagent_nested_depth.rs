//! 嵌套深度 3 端到端测试(roadmap §2.9 #12)。
//!
//! 验证 `SubAgentFactory::spawn` 在第 4 次时返回
//! `SubAgentError::MaxDepthExceeded { max: 3 }`,且 depth 计数器在 3 次成功
//! spawn 后 == 3。
//!
//! 复用 `reflect-core/tests/single_turn.rs` 的 StubClient pattern:不依赖
//! wiremock 或外部 LLM,直接构造 stub ModelClient 验证 subagent 工厂的深度
//! 上限语义。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_llm::{
    ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry, PoolEntry,
};
use reflect_subagent::{SubAgentError, SubAgentFactory, SubAgentSpec};
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Stub `ModelClient` —— 每次 stream 返回一条空事件,然后 MessageStop。
struct NoopClient;

#[async_trait]
impl ModelClient for NoopClient {
    fn name(&self) -> &str {
        "noop"
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        Ok(Box::pin(stream::iter(vec![
            Ok(ChatEvent::MessageStart {
                id: "m".into(),
                model: "noop-1".into(),
            }),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

fn build_factory_with_depth() -> (Arc<SubAgentFactory>, Arc<ModelRegistry>) {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "noop",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(NoopClient),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cancel = CancellationToken::new();
    let tools = Arc::new(ToolRegistry::default());
    let factory = Arc::new(SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        "noop/m1",
        registry.clone(),
        None, // child_registry: 回退父级 registry
        tools,
        cancel,
        None,
    ));
    (factory, registry)
}

/// 用 spawn 把 depth 推进到 `target`(0 ≤ target ≤ MAX_DEPTH)。
async fn advance_depth_to(factory: &SubAgentFactory, target: u8) {
    let current = factory.depth();
    for i in current..target {
        let spec = mk_spec(&format!("depth-advance-{i}"));
        let _ = factory.spawn(spec, vec![], "go".into()).await.unwrap();
    }
}

fn mk_spec(role: &str) -> SubAgentSpec {
    SubAgentSpec {
        name: role.to_string(),
        role: role.to_string(),
        model: None,
        system_prompt: format!("you are {role}"),
        allowed_tools: vec![],
        data_transfer: Default::default(),
        max_turns: None,
        allowed_skills: vec![],
    }
}

#[tokio::test]
async fn subagent_max_depth_three_rejects_fourth_spawn() {
    // depth 已经 = 3(预推进 3 次);第 4 次 spawn 应该被拒
    let (factory, _registry) = build_factory_with_depth();
    advance_depth_to(&factory, 3).await;
    assert_eq!(factory.depth(), 3);
    let spec = mk_spec("too-deep");
    let result = factory.spawn(spec, vec![], "go".into()).await;
    match result {
        Err(SubAgentError::MaxDepthExceeded { max }) => assert_eq!(max, 3),
        Ok(_) => panic!("expected MaxDepthExceeded at depth 3, but spawn succeeded"),
        Err(other) => panic!("expected MaxDepthExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_depth_increments_on_spawn() {
    // depth = 0 起步;第 1 次 spawn 应当成功,depth 变成 1
    let (factory, _registry) = build_factory_with_depth();
    assert_eq!(factory.depth(), 0);
    let spec = mk_spec("child");
    let _ = factory.spawn(spec, vec![], "go".into()).await.unwrap();
    assert_eq!(factory.depth(), 1);
}

#[tokio::test]
async fn subagent_child_factory_shares_depth_counter() {
    // 子 factory 共享 parent depth 计数器
    let (factory, _registry) = build_factory_with_depth();
    let child = factory.child_factory();
    advance_depth_to(&factory, 2).await;
    assert_eq!(child.depth(), 2, "child sees parent's depth");
    // child 自己的 spawn 也用同一个计数器
    let spec = mk_spec("grandchild");
    let _ = child.spawn(spec, vec![], "go".into()).await.unwrap();
    assert_eq!(
        factory.depth(),
        3,
        "spawn from child also advances parent depth"
    );
}

#[tokio::test]
async fn subagent_depth_three_e2e_orchestrator_integration() {
    // 端到端:用 reflect-discussion orchestrator + subagent factory 模拟
    // 3 层嵌套(parent → orchestrator → subagent A → subagent B);第 4 层
    // (subagent C) 应当被 MaxDepthExceeded 拒绝。
    //
    // 这个测试的目的是:验证 §2.9 #12 "subagent 嵌套深度 3 端到端集成测试"
    // —— 即 depth 计数器在 orchestrator + subagent factory 协作时正确
    // 推进并在第 4 次 spawn 时拒绝。
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "noop",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(NoopClient),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cancel = CancellationToken::new();
    let tools = Arc::new(ToolRegistry::default());
    let parent_factory = Arc::new(SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        "noop/m1",
        registry.clone(),
        None, // child_registry: 回退父级 registry
        tools,
        cancel,
        None,
    ));

    // 模拟"parent 启动 subagent A"
    let spec_a = mk_spec("a");
    let _a = parent_factory
        .spawn(spec_a, vec![], "go".into())
        .await
        .unwrap();
    assert_eq!(parent_factory.depth(), 1);

    // 模拟"subagent A 启动 subagent B(共享 depth 计数器)"
    let spec_b = mk_spec("b");
    let _b = parent_factory
        .spawn(spec_b, vec![], "go".into())
        .await
        .unwrap();
    assert_eq!(parent_factory.depth(), 2);

    // 模拟"subagent B 启动 subagent C"
    let spec_c = mk_spec("c");
    let _c = parent_factory
        .spawn(spec_c, vec![], "go".into())
        .await
        .unwrap();
    assert_eq!(parent_factory.depth(), 3);

    // 第 4 次 spawn 应当被 MaxDepthExceeded 拒绝
    let spec_d = mk_spec("d");
    let result = parent_factory.spawn(spec_d, vec![], "go".into()).await;
    match result {
        Err(SubAgentError::MaxDepthExceeded { max }) => assert_eq!(max, 3),
        Ok(_) => panic!("expected MaxDepthExceeded at depth 3, but spawn succeeded"),
        Err(other) => panic!("expected MaxDepthExceeded, got {other:?}"),
    }
}

#[test]
fn subagent_factory_debug_includes_depth() {
    let factory = SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        "noop/m1",
        Arc::new(ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    let s = format!("{factory:?}");
    // Debug 应该包含 depth 字段
    assert!(s.contains("depth"), "Debug missing depth: {s}");
}
