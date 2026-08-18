//! `steering_queue` —— Steering Queue(P2 `steering-queue`)。
//!
//! ATTACHMENT / NOW 双优先级用户消息注入;turn 边界 drain 高优先级队列。

use std::collections::VecDeque;

use reflect_protocol::UserInputItem;

///  steering 消息优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SteeringPriority {
    /// 普通附件,下一 safe point 注入。
    Attachment = 0,
    /// 立即打断当前 turn 规划。
    Now = 1,
}

/// 一条 steering 消息。
#[derive(Debug, Clone)]
pub struct SteeringMessage {
    pub priority: SteeringPriority,
    pub items: Vec<UserInputItem>,
}

/// 双优先级 steering 队列。
#[derive(Debug, Default)]
pub struct SteeringQueue {
    attachment: VecDeque<SteeringMessage>,
    now: VecDeque<SteeringMessage>,
}

impl SteeringQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队 steering 消息。
    pub fn push(&mut self, priority: SteeringPriority, items: Vec<UserInputItem>) {
        let msg = SteeringMessage { priority, items };
        match priority {
            SteeringPriority::Attachment => self.attachment.push_back(msg),
            SteeringPriority::Now => self.now.push_back(msg),
        }
    }

    /// 是否包含 NOW 优先级消息。
    pub fn has_now(&self) -> bool {
        !self.now.is_empty()
    }

    /// drain 全部 NOW 消息(高优先级优先)。
    pub fn drain_now(&mut self) -> Vec<SteeringMessage> {
        self.now.drain(..).collect()
    }

    /// drain 全部 ATTACHMENT 消息。
    pub fn drain_attachment(&mut self) -> Vec<SteeringMessage> {
        self.attachment.drain(..).collect()
    }

    /// 按优先级 drain:先 NOW 后 ATTACHMENT。
    pub fn drain_all(&mut self) -> Vec<SteeringMessage> {
        let mut out = self.drain_now();
        out.extend(self.drain_attachment());
        out
    }

    pub fn len(&self) -> usize {
        self.attachment.len() + self.now.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_drains_before_attachment() {
        let mut q = SteeringQueue::new();
        q.push(
            SteeringPriority::Attachment,
            vec![UserInputItem::Text {
                text: "attach".into(),
            }],
        );
        q.push(
            SteeringPriority::Now,
            vec![UserInputItem::Text { text: "now".into() }],
        );
        let drained = q.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].priority, SteeringPriority::Now);
    }

    #[test]
    fn has_now_reflects_queue_state() {
        let mut q = SteeringQueue::new();
        assert!(!q.has_now());
        q.push(SteeringPriority::Now, vec![]);
        assert!(q.has_now());
    }
}
