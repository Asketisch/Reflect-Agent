//! v1.4 A1 — `Op::Interrupt` 真中断端到端。
//!
//! 旧实现只发一条现编 turn_id 的 `TurnAborted`、不触碰任何取消令牌,
//! 正在跑的 turn 照常跑完。新实现:
//! 1. 每个 turn spawn 前从会话令牌派生回合级 child_token 并登记在飞回合表;
//! 2. `Op::Interrupt`(不带 child_id)cancel 表中所有令牌 —— 模型流
//!    select! / 工具 kill_on_cancel / 审批等待全部生效;
//! 3. 被取消的 turn 用**真实 turn_id**(与 `TurnStarted` 一致)发
//!    `TurnAborted`,不再出现 `TurnComplete`。
//!
//! 这里用「慢流模型」(事件间 sleep)驱动:中断落在模型流消费阶段,
//! 覆盖取消级联的主链路。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, Submission, UserInputItem};
use reflect_tools::ToolRegistry;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// 慢速 stub 模型:事件序列逐个发出,每个间隔 `interval`,模拟长思考流。
struct SlowClient {
    events: Vec<ChatEvent>,
    interval: Duration,
}

#[async_trait]
impl ModelClient for SlowClient {
    fn name(&self) -> &str {
        "slow-stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events = self.events.clone();
        let interval = self.interval;
        // unfold 的 sleep 在被 poll 时才推进 —— 消费循环的 select!(cancel)
        // 一旦触发,整条流被 drop,sleep 随之放弃,turn 走取消路径。
        Ok(Box::pin(stream::unfold(events, move |mut it| async move {
            if it.is_empty() {
                return None;
            }
            tokio::time::sleep(interval).await;
            let ev = it.remove(0);
            Some((Ok::<ChatEvent, LlmError>(ev), it))
        })))
    }
}

fn slow_pool(events: Vec<ChatEvent>) -> Arc<ModelRegistry> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "slow-stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(SlowClient {
                    events,
                    interval: Duration::from_millis(100),
                }),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    registry
}

/// 一条 3 秒左右的纯文本流(30 个 delta × 100ms)。
fn slow_text_events() -> Vec<ChatEvent> {
    let mut events = vec![ChatEvent::MessageStart {
        id: "slow-1".into(),
        model: "slow-stub".into(),
    }];
    for i in 0..30 {
        events.push(ChatEvent::ContentDelta(format!("chunk-{i} ")));
    }
    events.push(ChatEvent::MessageStop);
    events
}

fn sub(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
    }
}

fn build_thread(registry: Arc<ModelRegistry>) -> AgentThread {
    let cfg = AgentConfig::new("slow-stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    AgentThread::new(cfg, registry, tools, None, None)
}

/// 中断在飞 turn:turn 用真实 turn_id 发 TurnAborted,且不发 TurnComplete。
#[tokio::test]
async fn interrupt_cancels_in_flight_turn_with_real_turn_id() {
    let thread = build_thread(slow_pool(slow_text_events()));
    // 客户端复刻:需要第二条 submission 通道发 Interrupt。线程内部共享
    // sub_tx,直接用 thread.submit。
    let mut handle = thread
        .submit(sub(
            "t1",
            Op::UserInput {
                items: vec![UserInputItem::Text {
                    text: "写一篇长文".into(),
                }],
                thread_settings: Default::default(),
            },
        ))
        .await;

    // 1. 收 TurnStarted,记录真实 turn_id。
    let deadline = Duration::from_secs(5);
    let started_turn_id = loop {
        let ev = timeout(deadline, handle.next())
            .await
            .expect("turn event arrived")
            .expect("turn channel open");
        match ev.msg {
            EventMsg::TurnStarted(ts) => break ts.turn_id,
            EventMsg::SessionConfigured(_) => continue,
            other => panic!("unexpected event before TurnStarted: {other:?}"),
        }
    };

    // 2. 发 Interrupt(独立 submission;事件回执走它自己的通道,在飞
    //    turn 的 TurnAborted 走 t1 的通道)。
    let _interrupt_handle = thread
        .submit(sub("t1-int", Op::Interrupt { child_id: None }))
        .await;

    // 3. 在 t1 通道上等 TurnAborted:turn_id 必须等于 TurnStarted 的
    //    真实 id(旧实现是现编的新 id)。
    let mut saw_aborted = false;
    let mut saw_complete = false;
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event in time")
    {
        match ev.msg {
            EventMsg::TurnAborted(ab) => {
                assert_eq!(
                    ab.turn_id, started_turn_id,
                    "TurnAborted 必须携带在飞回合的真实 turn_id"
                );
                assert!(matches!(
                    ab.reason,
                    reflect_protocol::AbortReason::UserInterrupt
                ));
                saw_aborted = true;
            }
            EventMsg::TurnComplete(_) => {
                saw_complete = true;
            }
            _ => {}
        }
        // TurnAborted 是本 turn 的终态;TurnComplete 不应出现(即便晚到)。
        if saw_aborted {
            break;
        }
    }
    assert!(saw_aborted, "必须收到 TurnAborted");
    assert!(!saw_complete, "被中断的 turn 不应发 TurnComplete");

    // 4. 再等一小段确认没有迟到的 TurnComplete(取消后模型流已放弃)。
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(Some(ev)) = timeout(Duration::from_millis(50), handle.next()).await {
        assert!(
            !matches!(ev.msg, EventMsg::TurnComplete(_)),
            "中断后不应再有 TurnComplete"
        );
    }
}

/// 空闲期 Interrupt(无在飞 turn):保持旧回执行为,发一条 TurnAborted。
#[tokio::test]
async fn interrupt_when_idle_still_emits_receipt() {
    let thread = build_thread(slow_pool(slow_text_events()));
    let mut handle = thread
        .submit(sub("idle-int", Op::Interrupt { child_id: None }))
        .await;

    let ev = timeout(Duration::from_secs(2), handle.next())
        .await
        .expect("receipt arrived")
        .expect("channel open");
    match ev.msg {
        EventMsg::TurnAborted(ab) => {
            assert!(matches!(
                ab.reason,
                reflect_protocol::AbortReason::UserInterrupt
            ));
        }
        other => panic!("expected TurnAborted receipt, got {other:?}"),
    }
}

/// 子代理注册表经 cfg 接线:Interrupt 定向分支能查到注册表(此处仅验证
/// 配置透传与未知 child 的 no-op 路径;命中路径由 subagent_registry 单测覆盖)。
#[tokio::test]
async fn interrupt_with_child_id_routes_via_registry() {
    let runtime = Arc::new(reflect_core::SubagentRuntimeRegistry::new());
    let registry = slow_pool(vec![ChatEvent::MessageStop]);
    let cfg =
        AgentConfig::new("slow-stub/m1", Path::new(".")).with_subagent_runtime(runtime.clone());
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    // 未登记任何子代理:定向中断应安全 no-op(不 panic、不影响主循环)。
    let _handle = thread
        .submit(sub(
            "child-int",
            Op::Interrupt {
                child_id: Some("ghost-child".into()),
            },
        ))
        .await;

    // 主循环必须仍然存活(可处理后续 Shutdown)。
    let _shutdown = thread.submit(sub("child-int-shut", Op::Shutdown)).await;
    let mut session_rx = thread.subscribe_session();
    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("shutdown arrived")
        .expect("session channel open");
    assert!(matches!(ev.msg, EventMsg::ShutdownComplete));
}
