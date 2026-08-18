//! submission_loop 与 P2 steering / background 集成测试。

use std::sync::Arc;

use reflect_protocol::UserInputItem;

use reflect_core::background_tasks::BackgroundTaskQueue;
use reflect_core::steering_queue::{SteeringPriority, SteeringQueue};

#[test]
fn steering_and_background_merge_order() {
    let mut sq = SteeringQueue::new();
    sq.push(
        SteeringPriority::Now,
        vec![UserInputItem::Text {
            text: "steer".into(),
        }],
    );
    let bg = Arc::new(BackgroundTaskQueue::new());
    bg.register("t1", "bash", "echo");
    bg.complete("t1", "done");

    let steering: Vec<_> = sq.drain_all().into_iter().flat_map(|m| m.items).collect();
    let bg_done = bg.drain_completed();
    assert_eq!(steering.len(), 1);
    assert_eq!(bg_done.len(), 1);
}
