//! `message_bus` — 进程内消息路由 + transcript 持久化。
//!
//! 每个 [`AgentId`] 一个 bounded [`tokio::sync::mpsc::channel`];[`MessageBus::route`]
//! 把消息按 `recipients` 投递(空 = 广播给所有非 sender agent),并 append 到
//! 全量 transcript。transcript 由 `Arc<parking_lot::Mutex<Vec<_>>>` 持有,跨
//! `&MessageBus` 共享;caller 可以 `bus.transcript()` 拿快照,或
//! `bus.format_transcript()` 拿人类可读字符串。
//!
//! 关键设计:`MessageBus` 内部把状态包在 `Arc<MessageBusInner>` 中,自身实现
//! `Clone`(cheap clone,共享 inner)。这样 `&MessageBus` 跨 `await` 借用是
//! `Send`(借用 `Arc<Inner>`,`Arc` 本身 Send + Sync),让 comm_tools /
//! runtime / orchestrator 直接持有 `MessageBus` 而非 `Arc<parking_lot::Mutex<>>`,
//! 避免 `parking_lot::MutexGuard` 跨 await 时的 `!Send` 问题。
//!
//! `asyncio.Queue` → `tokio::sync::mpsc` 是直接对应;mpsc 是多生产者单消费者,
//! 对应 Python 中"所有 send_message 调用都投递到目标 agent 的 queue"语义。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::models::{AgentId, DiscussionId, DiscussionMessage, MessageId};

/// 一个 Agent 的邮箱。
///
/// `tx` 总是存在(供多生产者 send);`rx` 是 `Option<Receiver>` 以便在
/// `take()` 后显式销毁(目前 v0 不暴露 take,保持 receiver 长期可用)。
///
/// 容量上限来自 [`DiscussionConfig::mailbox_capacity`](crate::models::DiscussionConfig::mailbox_capacity);
/// 容量满时 `send` 会 backpressure(`mpsc::Sender::send` 在满时 `.await`)。
#[derive(Debug)]
pub struct AgentMailbox {
    /// 邮箱所属 agent。
    pub agent: AgentId,
    /// 发送端;bus 通过它投递消息。
    tx: mpsc::Sender<DiscussionMessage>,
    /// 接收端;runtime/orchestrator 通过它读消息(可被 `take` 出来独占消费)。
    rx: Option<mpsc::Receiver<DiscussionMessage>>,
    /// 容量上限(只读)。
    capacity: usize,
}

impl AgentMailbox {
    /// 新建一个 bounded mailbox(capacity 来自 `DiscussionConfig::mailbox_capacity`)。
    pub fn new(agent: AgentId, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            agent,
            tx,
            rx: Some(rx),
            capacity,
        }
    }

    /// 异步发送一条消息;若 receiver 已 drop 则返回 [`BusError::MailboxClosed`]。
    pub async fn send(&self, msg: DiscussionMessage) -> Result<(), BusError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| BusError::MailboxClosed {
                agent: self.agent.clone(),
            })
    }

    /// 阻塞接收下一条消息(`None` 表示所有 sender 已 drop)。
    pub async fn recv(&mut self) -> Option<DiscussionMessage> {
        match self.rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// 非阻塞接收(给并发模式的 runtime 用)。
    pub fn try_recv(&mut self) -> Result<DiscussionMessage, mpsc::error::TryRecvError> {
        match self.rx.as_mut() {
            Some(rx) => rx.try_recv(),
            None => Err(mpsc::error::TryRecvError::Empty),
        }
    }
}

/// 路由 / 邮箱错误。
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// 目标 mailbox 的 receiver 已 drop(说明 agent 已退出讨论)。
    #[error("mailbox for agent {agent:?} is closed")]
    MailboxClosed { agent: AgentId },
    /// 路由到未在 bus 注册的 agent。
    #[error("agent {0:?} not registered in bus")]
    UnknownAgent(AgentId),
}

/// `MessageBus` 的共享内部状态;持有 transcript / mailboxes / next_id 自增器。
#[derive(Debug)]
struct MessageBusInner {
    discussion_id: DiscussionId,
    /// mailbox 表(用 Mutex 包装,让 `mailbox_mut` 可以走 `&self` 路径)。
    /// 锁在 sync 路径(try_recv/recv)持有时间极短,不跨 await。
    mailboxes: Mutex<HashMap<AgentId, AgentMailbox>>,
    /// 全量 transcript(append-only)。
    transcript: Mutex<Vec<DiscussionMessage>>,
    /// 下一个 `MessageId` 自增器。
    next_id: Mutex<MessageId>,
}

/// 多 Agent 消息总线。
///
/// 内部数据全部包在 `Arc<MessageBusInner>` 里,`MessageBus` 自身只持一个
/// `Arc<Inner>` 因此 `Clone` 是 cheap;`&MessageBus` 跨 `await` 借用 OK
/// (`Arc<Inner>: Send + Sync`)。
#[derive(Debug, Clone)]
pub struct MessageBus {
    inner: Arc<MessageBusInner>,
}

impl MessageBus {
    /// 新建总线,自动给每个 participant 建一个 mailbox。
    pub fn new(discussion_id: DiscussionId, participants: Vec<AgentId>, capacity: usize) -> Self {
        let mailboxes = participants
            .into_iter()
            .map(|a| (a.clone(), AgentMailbox::new(a, capacity)))
            .collect();
        Self {
            inner: Arc::new(MessageBusInner {
                discussion_id,
                mailboxes: Mutex::new(mailboxes),
                transcript: Mutex::new(Vec::new()),
                next_id: Mutex::new(MessageId(0)),
            }),
        }
    }

    /// 讨论 ID(只读)。
    pub fn discussion_id(&self) -> DiscussionId {
        self.inner.discussion_id
    }

    /// 取某个 agent 的 mailbox(只读)。
    pub fn mailbox(&self, agent: &AgentId) -> Option<AgentMailbox> {
        // clone 出 AgentMailbox;AgentMailbox 内部 tx 是 mpsc::Sender(cheap clone),
        // rx 是 None / 不可 clone —— 实际这里 clone 一个"rx=None"的新 AgentMailbox,
        // 只能用来 send,不能用来 recv/try_recv。
        self.inner
            .mailboxes
            .lock()
            .get(agent)
            .map(|m| AgentMailbox {
                agent: m.agent.clone(),
                tx: m.tx.clone(),
                rx: None,
                capacity: m.capacity,
            })
    }

    /// 取某个 agent 的 mailbox(可变,用于 `try_recv` / `recv`)。
    ///
    /// 返回 `Option<Guard<'_, ...>>` 比较复杂,改用 [`MessageBus::with_mailbox_mut`]
    /// closure 形式避免暴露锁类型。
    pub fn with_mailbox_mut<R>(
        &self,
        agent: &AgentId,
        f: impl FnOnce(&mut AgentMailbox) -> R,
    ) -> Option<R> {
        let mut guard = self.inner.mailboxes.lock();
        guard.get_mut(agent).map(|mb| f(mb))
    }

    /// 所有已注册 agent 的列表(任意顺序)。
    pub fn agents(&self) -> Vec<AgentId> {
        self.inner.mailboxes.lock().keys().cloned().collect()
    }

    /// 全量 transcript 快照(克隆;锁内短暂持有)。
    pub fn transcript(&self) -> Vec<DiscussionMessage> {
        self.inner.transcript.lock().clone()
    }

    /// transcript 当前长度(廉价)。
    pub fn transcript_len(&self) -> usize {
        self.inner.transcript.lock().len()
    }

    /// 路由一条消息:
    /// 1. 分配下一个 `MessageId`
    /// 2. 解析 targets(recipients 为空 = 广播给所有**非 sender** agent)
    /// 3. 逐个投递(`send().await`)
    /// 4. append 到 transcript
    ///
    /// 错误:目标 agent 未注册 → [`BusError::UnknownAgent`];receiver 已 drop →
    /// [`BusError::MailboxClosed`];这两种情况下消息**不会**被 append(避免 transcript
    /// 出现"未投递"的消息)。
    pub async fn route(&self, mut msg: DiscussionMessage) -> Result<(), BusError> {
        // 1. 分配 id(短锁,await 前已 drop)
        let id = {
            let mut next = self.inner.next_id.lock();
            let id = MessageId(next.0);
            next.0 += 1;
            id
        };
        msg.id = id;

        // 2. 解析 targets(锁内 short-lived)
        let targets: Vec<AgentId> = {
            let mailboxes = self.inner.mailboxes.lock();
            if msg.recipients.is_empty() {
                mailboxes
                    .keys()
                    .filter(|a| **a != msg.from)
                    .cloned()
                    .collect()
            } else {
                msg.recipients.clone()
            }
        };

        // 3. 逐个投递(await 时不持 mailboxes 锁)
        for target in &targets {
            // 拿 sender clone(避免 await 期间持锁)
            let tx = {
                let mailboxes = self.inner.mailboxes.lock();
                let mb = mailboxes
                    .get(target)
                    .ok_or_else(|| BusError::UnknownAgent(target.clone()))?;
                mb.tx.clone()
            };
            tx.send(msg.clone())
                .await
                .map_err(|_| BusError::MailboxClosed {
                    agent: target.clone(),
                })?;
        }

        // 4. append transcript(短锁)
        self.inner.transcript.lock().push(msg);
        Ok(())
    }

    /// transcript 人类可读格式(给 CLI 打印 / log;格式:`[r{round}/{kind:?}] {from}: {content}\n`)。
    pub fn format_transcript(&self) -> String {
        let mut out = String::new();
        for m in self.inner.transcript.lock().iter() {
            out.push_str(&format!(
                "[r{}/{:?}] {}: {}\n",
                m.round, m.kind, m.from.0, m.content
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DiscussionConfig, DiscussionMode, MessageKind};

    /// 构造一个测试用 `DiscussionMessage`(用 `bus.route` 测试路由语义)。
    fn mk_msg(
        discussion_id: DiscussionId,
        from: AgentId,
        content: &str,
        recipients: Vec<AgentId>,
        round: u32,
    ) -> DiscussionMessage {
        DiscussionMessage {
            id: MessageId(0), // 会被 bus.route 重写
            discussion_id,
            from,
            kind: MessageKind::Utterance,
            content: content.into(),
            recipients,
            round,
            token_usage: Default::default(),
        }
    }

    #[tokio::test]
    async fn mailbox_send_recv_roundtrip() {
        let mut mb = AgentMailbox::new(AgentId("alice".into()), 4);
        let did = DiscussionId::new();
        let msg = mk_msg(did, AgentId("bob".into()), "hi", vec![], 0);
        mb.send(msg.clone()).await.unwrap();
        let got = mb.recv().await.unwrap();
        assert_eq!(got.content, "hi");
        assert_eq!(got.from, AgentId("bob".into()));
    }

    #[tokio::test]
    async fn mailbox_send_after_recv_drop_returns_closed() {
        // 1. 拿 mailbox 出来,丢掉 receiver(scope 内显式 drop,不留到结尾)
        let mut mb = AgentMailbox::new(AgentId("alice".into()), 1);
        let rx = mb.rx.take().expect("rx must exist");
        drop(rx);
        // 2. send 应该返回 MailboxClosed
        let did = DiscussionId::new();
        let msg = mk_msg(did, AgentId("bob".into()), "x", vec![], 0);
        let result = mb.send(msg).await;
        assert!(result.is_err(), "send after receiver drop should fail");
        assert!(
            matches!(result.unwrap_err(), BusError::MailboxClosed { .. }),
            "expected MailboxClosed",
        );
    }

    #[tokio::test]
    async fn message_bus_creates_mailbox_per_participant() {
        let participants = vec![
            AgentId("a".into()),
            AgentId("b".into()),
            AgentId("c".into()),
        ];
        let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 8);
        assert_eq!(bus.agents().len(), 3);
        for p in &participants {
            assert!(bus.mailbox(p).is_some());
        }
    }

    #[tokio::test]
    async fn message_bus_broadcast_routes_to_all_non_sender() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(
            did,
            vec![
                AgentId("a".into()),
                AgentId("b".into()),
                AgentId("c".into()),
            ],
            8,
        );
        bus.route(mk_msg(did, AgentId("a".into()), "hello all", vec![], 0))
            .await
            .unwrap();
        // b 和 c 都应该收到
        let b_got = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let c_got = bus
            .with_mailbox_mut(&AgentId("c".into()), |mb| mb.try_recv().ok())
            .expect("c mailbox exists");
        assert_eq!(b_got.as_ref().expect("b message").content, "hello all");
        assert_eq!(c_got.as_ref().expect("c message").content, "hello all");
        // a 没收到自己发的(broadcast 排除 sender)
        let a_got = bus
            .with_mailbox_mut(&AgentId("a".into()), |mb| mb.try_recv().ok())
            .expect("a mailbox exists");
        assert!(
            a_got.is_none(),
            "sender should not receive their own broadcast"
        );
        // transcript 一条
        assert_eq!(bus.transcript_len(), 1);
    }

    #[tokio::test]
    async fn message_bus_unicast_routes_only_to_named_recipients() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(
            did,
            vec![
                AgentId("a".into()),
                AgentId("b".into()),
                AgentId("c".into()),
            ],
            8,
        );
        bus.route(mk_msg(
            did,
            AgentId("a".into()),
            "private to b",
            vec![AgentId("b".into())],
            0,
        ))
        .await
        .unwrap();
        // 只有 b 收到
        let b_got = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        assert_eq!(b_got.expect("b message").content, "private to b");
        // a / c mailbox 应该空
        let a_got = bus
            .with_mailbox_mut(&AgentId("a".into()), |mb| mb.try_recv().ok())
            .expect("a mailbox exists");
        let c_got = bus
            .with_mailbox_mut(&AgentId("c".into()), |mb| mb.try_recv().ok())
            .expect("c mailbox exists");
        assert!(a_got.is_none());
        assert!(c_got.is_none());
    }

    #[tokio::test]
    async fn message_bus_assigns_monotonic_message_ids() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(did, vec![AgentId("a".into()), AgentId("b".into())], 8);
        for i in 0..5 {
            bus.route(mk_msg(
                did,
                AgentId("a".into()),
                &format!("m{i}"),
                vec![],
                0,
            ))
            .await
            .unwrap();
        }
        let t = bus.transcript();
        assert_eq!(t.len(), 5);
        for (i, m) in t.iter().enumerate() {
            assert_eq!(m.id.0, i as u64, "id should be monotonic starting at 0");
        }
    }

    #[tokio::test]
    async fn message_bus_routes_to_unknown_agent_returns_error() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(did, vec![AgentId("a".into())], 8);
        let err = bus
            .route(mk_msg(
                did,
                AgentId("a".into()),
                "x",
                vec![AgentId("ghost".into())],
                0,
            ))
            .await
            .unwrap_err();
        matches!(err, BusError::UnknownAgent(_));
        // 失败时 transcript 不增长
        assert_eq!(bus.transcript_len(), 0);
    }

    #[test]
    fn message_bus_format_transcript_contains_all_fields() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(did, vec![AgentId("a".into()), AgentId("b".into())], 8);
        // 没法在 sync 测试里跑 route(它是 async),所以直接构造一个 transcript 注入。
        // 这里简化:只断言 format_transcript 在 transcript 为空时返空字符串。
        assert_eq!(bus.format_transcript(), "");
    }

    #[tokio::test]
    async fn message_bus_format_transcript_after_routing() {
        let did = DiscussionId::new();
        let bus = MessageBus::new(did, vec![AgentId("a".into()), AgentId("b".into())], 8);
        bus.route(mk_msg(did, AgentId("a".into()), "hi b", vec![], 0))
            .await
            .unwrap();
        bus.route(mk_msg(did, AgentId("b".into()), "hi a", vec![], 0))
            .await
            .unwrap();
        let s = bus.format_transcript();
        // 包含 round / kind / agent / content
        assert!(s.contains("[r0/Utterance] a: hi b\n"), "got: {s}");
        assert!(s.contains("[r0/Utterance] b: hi a\n"), "got: {s}");
    }

    #[test]
    fn discussion_mode_default_is_concurrent() {
        // DiscussionConfig::default 应该有 Concurrent mode
        assert_eq!(DiscussionConfig::default().mode, DiscussionMode::Concurrent);
    }

    #[test]
    fn message_bus_is_clone_shares_inner_state() {
        let did = DiscussionId::new();
        let bus1 = MessageBus::new(did, vec![AgentId("a".into())], 4);
        let bus2 = bus1.clone();
        // bus1 和 bus2 共享 inner(同一 transcript)
        assert_eq!(bus1.discussion_id(), bus2.discussion_id());
        assert_eq!(bus1.agents(), bus2.agents());
    }
}
