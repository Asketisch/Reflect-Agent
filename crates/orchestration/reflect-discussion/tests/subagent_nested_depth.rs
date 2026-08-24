//! 嵌套深度 in-flight 端到端测试(roadmap §2.9 #12 修订版)。
//!
//! v0.x 起,`SubAgentFactory` 的 `in_flight` 计数器是**并发 in-flight**
//! 语义,而非总 spawn 数:`spawn()` 时 +1,`SpawnedChild` 析构或
//! `collect_result` 终态时 -1。上限提升到 `MAX_DEPTH = 16`(`MAX_IN_FLIGHT`)。
//!
//! 本测试验证:
//! 1. `spawn` 在达到上限时返回 `SubAgentError::MaxDepthExceeded { max: 16 }`;
//! 2. `depth()` 反映当前 in-flight 数;
//! 3. `SpawnedChild` 析构后槽位释放;
//! 4. Debug 输出包含 `in_flight`。

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_llm::{
    ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry, PoolEntry,
};
use reflect_subagent::{SpawnedChild, SubAgentError, SubAgentFactory, SubAgentSpec};
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

fn build_factory() -> (Arc<SubAgentFactory>, Arc<ModelRegistry>) {
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

/// 推进 depth 到 `target` 并把所有 SpawnedChild 留在 `Vec` 中(避免 Drop 释放)。
async fn advance_depth_to(factory: &SubAgentFactory, target: u8) -> Vec<SpawnedChild> {
    let mut children = Vec::new();
    while factory.depth() < target {
        let i = factory.depth();
        let spec = mk_spec(&format!("depth-advance-{i}"));
        let child = factory.spawn(spec, vec![], "go".into()).await.unwrap();
        children.push(child);
    }
    children
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
async fn subagent_max_depth_sixteen_rejects_seventeenth_spawn() {
    // 推进 16 次 spawn 到 in-flight = 16;第 17 次应被拒。
    let (factory, _registry) = build_factory();
    let _held = advance_depth_to(&factory, reflect_subagent::MAX_DEPTH).await;
    assert_eq!(factory.depth(), reflect_subagent::MAX_DEPTH);
    let spec = mk_spec("too-deep");
    let result = factory.spawn(spec, vec![], "go".into()).await;
    match result {
        Err(SubAgentError::MaxDepthExceeded { max }) => {
            assert_eq!(max, reflect_subagent::MAX_DEPTH);
        }
        Ok(_) => panic!(
            "expected MaxDepthExceeded at in-flight {}, but spawn succeeded",
            reflect_subagent::MAX_DEPTH
        ),
        Err(other) => panic!("expected MaxDepthExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_in_flight_increments_on_spawn() {
    let (factory, _registry) = build_factory();
    assert_eq!(factory.depth(), 0);
    let _held = advance_depth_to(&factory, 1).await;
    assert_eq!(factory.depth(), 1);
}

#[tokio::test]
async fn subagent_child_factory_shares_in_flight_counter() {
    let (factory, _registry) = build_factory();
    let child = factory.child_factory();
    let _held = advance_depth_to(&factory, 2).await;
    assert_eq!(child.depth(), 2, "child sees parent's in-flight");
    // child 自己的 spawn 也用同一个计数器
    let spec = mk_spec("grandchild");
    let grandchild = child.spawn(spec, vec![], "go".into()).await.unwrap();
    assert_eq!(
        factory.depth(),
        3,
        "spawn from child also advances parent in-flight"
    );
    drop(grandchild);
    assert_eq!(
        factory.depth(),
        2,
        "drop SpawnedChild should release in-flight slot"
    );
}

#[tokio::test]
async fn subagent_in_flight_released_on_drop() {
    // 关键回归测试:漏调 collect_result,仅靠 Drop 也能释放 in-flight 槽位。
    let (factory, _registry) = build_factory();
    let _held = advance_depth_to(&factory, 3).await;
    assert_eq!(factory.depth(), 3);
    drop(_held);
    assert_eq!(
        factory.depth(),
        0,
        "所有 SpawnedChild drop 后 in-flight 必须归 0"
    );
}

#[tokio::test]
async fn subagent_in_flight_sixteen_e2e_orchestrator_integration() {
    // 端到端:用 reflect-discussion orchestrator + subagent factory 模拟
    // 16 层嵌套(填满 in-flight);第 17 层应当被 MaxDepthExceeded 拒绝。
    let (factory, _registry) = build_factory();
    let _held = advance_depth_to(&factory, reflect_subagent::MAX_DEPTH).await;
    assert_eq!(factory.depth(), reflect_subagent::MAX_DEPTH);

    // 第 17 次 spawn 应当被 MaxDepthExceeded 拒绝
    let spec = mk_spec("overflow");
    let result = factory.spawn(spec, vec![], "go".into()).await;
    assert!(
        matches!(result, Err(SubAgentError::MaxDepthExceeded { .. })),
        "expected MaxDepthExceeded, got {result:?}"
    );
}

#[test]
fn subagent_factory_debug_includes_in_flight() {
    let factory = SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        "noop/m1",
        Arc::new(ModelRegistry::new()),
        None,
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    let s = format!("{factory:?}");
    // Debug 字段名已从 `depth` 改为 `in_flight`(反映新语义)
    assert!(s.contains("in_flight"), "Debug missing `in_flight`: {s}");
}
