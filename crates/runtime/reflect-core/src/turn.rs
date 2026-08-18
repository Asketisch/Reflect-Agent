//! `TurnHandle` — 对全局事件流的每轮订阅。

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tokio::sync::mpsc;

use reflect_protocol::Event;

/// 由 `AgentThread::submit()` 返回的句柄。receiver 持续产出事件,
/// 直到每轮 channel 关闭(submission loop 从 `turn_subs` 中移除条目),
/// 此时迭代器返回 `None`。
///
/// `TurnHandle` 实现了 [`futures::Stream`],因而可用全套 `StreamExt`
/// 组合子(`map`、`filter`、`take_while` 等)。内建的 [`TurnHandle::next`]
/// 为不愿导入 `StreamExt` 的调用方保留。
pub struct TurnHandle {
    rx: mpsc::Receiver<Event>,
}

impl TurnHandle {
    pub(crate) fn new(rx: mpsc::Receiver<Event>) -> Self {
        Self { rx }
    }

    /// 公开构造器,供 `reflect::stream::EventStream` 测试使用
    /// (无法触及 crate-private 的 `new`)。
    #[doc(hidden)]
    pub fn from_receiver_for_test(rx: mpsc::Receiver<Event>) -> Self {
        Self::new(rx)
    }

    /// 等待下一个事件。turn 结束后返回 `None`。
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

impl Stream for TurnHandle {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use reflect_protocol::{EVENT_ID_NONE, EventMsg};

    fn make_event(text: &str) -> Event {
        Event::new(
            EVENT_ID_NONE,
            EventMsg::AgentMessageDelta(reflect_protocol::AgentMessageDelta { delta: text.into() }),
        )
    }

    #[tokio::test]
    async fn stream_impl_yields_in_order() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut h = TurnHandle::new(rx);
        tx.send(make_event("a")).await.unwrap();
        tx.send(make_event("b")).await.unwrap();
        drop(tx);

        let collected: Vec<String> = h
            .by_ref()
            .map(|ev| match ev.msg {
                EventMsg::AgentMessageDelta(d) => d.delta,
                _ => String::new(),
            })
            .collect()
            .await;
        assert_eq!(collected, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn stream_impl_returns_none_on_close() {
        let (tx, rx) = mpsc::channel::<Event>(1);
        let mut h = TurnHandle::new(rx);
        drop(tx);
        assert!(h.next().await.is_none());
        // Subsequent polls keep returning None.
        assert!(StreamExt::next(&mut h).await.is_none());
    }

    #[tokio::test]
    async fn inherent_next_and_stream_next_compose() {
        let (tx, rx) = mpsc::channel::<Event>(2);
        let mut h = TurnHandle::new(rx);
        tx.send(make_event("first")).await.unwrap();
        tx.send(make_event("second")).await.unwrap();
        drop(tx);

        // 先用固有 next() 再用 StreamExt::next(),应按顺序排空。
        let a = h.next().await.unwrap();
        let b = StreamExt::next(&mut h).await.unwrap();
        let c = h.next().await;
        assert!(matches!(a.msg, EventMsg::AgentMessageDelta(ref d) if d.delta == "first"));
        assert!(matches!(b.msg, EventMsg::AgentMessageDelta(ref d) if d.delta == "second"));
        assert!(c.is_none());
    }
}
