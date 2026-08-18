//! `EventStream` —— 把一个 `TurnHandle` 与可选的 session 事件订阅
//! 合并成单个 `futures::Stream`。
//!
//! 库使用者只需 `await stream.next()` 一次,即可收到来自本轮通道和
//! session 级订阅两端的事件(谁先到谁先收)。

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use reflect_core::TurnHandle;
use reflect_protocol::Event;
use tokio::sync::mpsc;

/// Reflect thread 抛出的 `Event` 流,可选择与该 thread 的 session 事件
/// 订阅者合并。
///
/// 可用 [`EventStream::next`](自带的,因 `StreamExt` 让深度用户更顺手)
/// 或 `futures::Stream` impl 拿到完整 combinator 表面。
pub struct EventStream {
    /// 本轮通道(由 `TurnHandle` 产出)。
    turn: TurnHandle,
    /// 可选的 session 事件订阅者;先于 `turn` poll,以便 `SessionConfigured`
    /// 能最早被看到。
    session: Option<mpsc::Receiver<Event>>,
}

impl EventStream {
    /// 把 `Reflect::submit` 返回的 `TurnHandle` 包成不带 session 事件的 stream。
    pub fn new(handle: TurnHandle) -> Self {
        Self {
            turn: handle,
            session: None,
        }
    }

    /// 把 `TurnHandle` 与一个 session 订阅者一起打包,返回的 stream 同时
    /// 产出本轮事件和 session 级事件。
    pub fn with_session(handle: TurnHandle, session: mpsc::Receiver<Event>) -> Self {
        Self {
            turn: handle,
            session: Some(session),
        }
    }

    /// 便捷方法:`tokio::select!` 同时监听 session + turn。返回任一源
    /// 的下一个事件;若 session 通道关闭则丢弃之。
    pub async fn next(&mut self) -> Option<Event> {
        if let Some(sess) = self.session.as_mut() {
            tokio::select! {
                biased;
                ev = sess.recv() => {
                    if ev.is_none() {
                        // Session 通道关闭 —— 丢弃并 fall through。
                        self.session = None;
                    }
                    if ev.is_some() {
                        return ev;
                    }
                }
                ev = self.turn.next() => return ev,
            }
        }
        self.turn.next().await
    }
}

impl Stream for EventStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(sess) = self.session.as_mut() {
            match sess.poll_recv(cx) {
                Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                Poll::Ready(None) => {
                    // Session 通道关闭;丢弃并继续从 turn 读。
                    self.session = None;
                }
                Poll::Pending => {} // fall through 到 turn
            }
        }
        Pin::new(&mut self.turn).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::EVENT_ID_NONE;

    #[tokio::test]
    async fn next_returns_turn_event() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let turn = TurnHandle::from_receiver_for_test(rx);
        let mut s = EventStream::new(turn);
        tx.send(Event::new(
            EVENT_ID_NONE.to_string(),
            reflect_protocol::EventMsg::TurnStarted(reflect_protocol::TurnStartedEvent {
                turn_id: reflect_protocol::TurnId::new(),
                user_message_id: None,
            }),
        ))
        .await
        .unwrap();
        drop(tx);
        let ev = s.next().await.unwrap();
        assert!(matches!(ev.msg, reflect_protocol::EventMsg::TurnStarted(_)));
    }

    #[tokio::test]
    async fn with_session_yields_session_then_turn() {
        let (sess_tx, sess_rx) = mpsc::channel::<Event>(4);
        let (turn_tx, turn_rx) = mpsc::channel::<Event>(4);
        let turn = TurnHandle::from_receiver_for_test(turn_rx);
        let mut s = EventStream::with_session(turn, sess_rx);

        sess_tx
            .send(Event::new(
                EVENT_ID_NONE.to_string(),
                reflect_protocol::EventMsg::SessionConfigured(
                    reflect_protocol::SessionConfiguredEvent::new("m", "p"),
                ),
            ))
            .await
            .unwrap();
        turn_tx
            .send(Event::new(
                "1".to_string(),
                reflect_protocol::EventMsg::TurnStarted(reflect_protocol::TurnStartedEvent {
                    turn_id: reflect_protocol::TurnId::new(),
                    user_message_id: None,
                }),
            ))
            .await
            .unwrap();

        let first = s.next().await.unwrap();
        assert!(matches!(
            first.msg,
            reflect_protocol::EventMsg::SessionConfigured(_)
        ));
        let second = s.next().await.unwrap();
        assert!(matches!(
            second.msg,
            reflect_protocol::EventMsg::TurnStarted(_)
        ));
    }
}
