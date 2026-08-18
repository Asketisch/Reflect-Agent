//! M6.1.2 — `AgentThread::subscribe_session` 对 `SessionConfigured` 与
//! `ShutdownComplete` 的事件分发。事件仍继续到达每 turn channel;
//! session 订阅者是给不愿把生命周期事件绑到具体提交的 TUI 消费者
//! 提供的额外 sink。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, Submission, UserInputItem};
use reflect_tools::ToolRegistry;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct StubClient {
    events: Mutex<Vec<ChatEvent>>,
}

impl StubClient {
    fn new(events: Vec<ChatEvent>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }
}

#[async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events = self.events.lock().clone();
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn build_thread(registry: Arc<ModelRegistry>) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    AgentThread::new(cfg, registry, tools, None, None)
}

fn user_input_sub(id: &str, text: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
    }
}

fn shutdown_sub(id: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::Shutdown,
        client_user_message_id: None,
        trace: None,
    }
}

#[tokio::test]
async fn subscribe_session_receives_session_configured() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::ContentDelta("hi".into()),
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s1", "hi")).await;

    // SessionConfigured 必须送达 session 订阅者,即使它也会进入 per-turn 通道。
    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("session event arrived within 2s")
        .expect("session channel still open");
    assert!(
        matches!(ev.msg, EventMsg::SessionConfigured(_)),
        "got {:?}",
        ev.msg
    );
}

#[tokio::test]
async fn subscribe_session_receives_shutdown_complete() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(shutdown_sub("s-shut")).await;

    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("shutdown event arrived")
        .expect("channel open");
    assert!(
        matches!(ev.msg, EventMsg::ShutdownComplete),
        "got {:?}",
        ev.msg
    );
}

#[tokio::test]
async fn multiple_subscribers_each_get_the_event() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut a = thread.subscribe_session();
    let mut b = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s2", "hi")).await;

    let ev_a = timeout(Duration::from_secs(2), a.recv())
        .await
        .unwrap()
        .unwrap();
    let ev_b = timeout(Duration::from_secs(2), b.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev_a.msg, EventMsg::SessionConfigured(_)));
    assert!(matches!(ev_b.msg, EventMsg::SessionConfigured(_)));
}

#[tokio::test]
async fn session_configured_emitted_only_once_per_thread() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _h1 = thread.submit(user_input_sub("t1", "hi")).await;
    let _h2 = thread.submit(user_input_sub("t2", "hi again")).await;

    // 第一个事件必须是 SessionConfigured。
    let ev1 = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev1.msg, EventMsg::SessionConfigured(_)));

    // 第二次提交不再产生 SessionConfigured。
    let next = timeout(Duration::from_millis(300), session_rx.recv()).await;
    match next {
        Err(_) => {} // timeout — expected: nothing else session-level
        Ok(Some(ev)) => assert!(
            !matches!(ev.msg, EventMsg::SessionConfigured(_)),
            "second SessionConfigured leaked: {:?}",
            ev.msg
        ),
        Ok(None) => {} // channel closed — also acceptable
    }
}
