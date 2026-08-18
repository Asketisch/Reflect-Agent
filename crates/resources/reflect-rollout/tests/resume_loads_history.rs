//! 端到端测试:写入全部 4 种记录类型,经 writer 的
//! `replay()` 方法回放,断言重建的历史与原始一致。

use reflect_protocol::{MessageRole, RolloutRecord, RolloutRecorder, ThreadId, TurnId};
use reflect_rollout::JsonlRolloutWriter;
use tempfile::tempdir;

#[tokio::test]
async fn roundtrips_all_four_record_types() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);

    let tid = TurnId::new();
    let records = vec![
        RolloutRecord::session_meta(sid, "anthropic/claude-3-5-sonnet-latest"),
        RolloutRecord::message(tid, MessageRole::User, serde_json::json!("hi")),
        RolloutRecord::message(tid, MessageRole::Assistant, serde_json::json!("hello")),
        RolloutRecord::Compaction {
            turn_id: tid,
            strategy: "microcompact".into(),
            removed_count: 8,
            summary: "<summary>...</summary>".into(),
        },
        RolloutRecord::Fork {
            parent_session_id: sid,
            branch_name: "explorer".into(),
        },
    ];
    for r in &records {
        writer.record(r.clone()).await.unwrap();
    }

    let back = writer.replay(sid).await.unwrap();
    assert_eq!(back.len(), records.len());
    for (a, b) in records.iter().zip(back.iter()) {
        assert_eq!(a, b, "mismatch for {a:?} vs {b:?}");
    }
}

#[tokio::test]
async fn empty_writer_replay_returns_empty() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);
    let back = writer.replay(sid).await.unwrap();
    assert!(back.is_empty());
}
