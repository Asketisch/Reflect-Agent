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
            before_tokens: 0,
            after_tokens: 0,
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

/// 旧日期会话及轮转副本必须按原始消息顺序回放,恢复后继续写原文件。
#[tokio::test]
async fn resumes_old_rotated_session_by_both_ids() {
    let dir = tempdir().unwrap();
    let thread_id = ThreadId::new();
    let session_id = ThreadId::new();
    let day = dir.path().join("2020/01/02");
    std::fs::create_dir_all(&day).unwrap();
    let meta = RolloutRecord::session_meta(session_id, "mock");
    let mut expected = Vec::new();
    for (suffix, text) in [(".3", "oldest"), (".1", "middle"), ("", "newest")] {
        let message =
            RolloutRecord::message(TurnId::new(), MessageRole::User, serde_json::json!(text));
        std::fs::write(
            day.join(format!("{thread_id}{suffix}.jsonl")),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta).unwrap(),
                serde_json::to_string(&message).unwrap()
            ),
        )
        .unwrap();
        expected.extend([meta.clone(), message]);
    }
    for id in [thread_id, session_id] {
        assert_eq!(
            reflect_rollout::replay(dir.path(), id).await.unwrap(),
            expected
        );
    }
    let writer = JsonlRolloutWriter::new(dir.path(), session_id);
    assert_eq!(writer.replay(session_id).await.unwrap(), expected);
    let added = RolloutRecord::message(
        TurnId::new(),
        MessageRole::Assistant,
        serde_json::json!("continued"),
    );
    writer.record(added.clone()).await.unwrap();
    expected.push(added);
    assert_eq!(writer.replay(session_id).await.unwrap(), expected);
    let active = std::fs::read_to_string(day.join(format!("{thread_id}.jsonl"))).unwrap();
    assert!(active.contains("continued"));
    assert_eq!(
        reflect_rollout::index::find_files_by_name_prefix(dir.path(), &session_id.to_string())
            .unwrap()
            .len(),
        0
    );
}

/// 写入触发真实轮转后,回放不能只剩活跃文件的尾部消息。
#[tokio::test]
async fn replay_retains_messages_after_rotation() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);
    writer
        .record(RolloutRecord::session_meta(sid, "mock"))
        .await
        .unwrap();
    let mut expected = Vec::new();
    for i in 0..200 {
        let message = RolloutRecord::message(
            TurnId::new(),
            MessageRole::Assistant,
            serde_json::json!({"i": i, "blob": "x".repeat(2048)}),
        );
        writer.record(message.clone()).await.unwrap();
        expected.push(message);
    }
    let messages: Vec<_> = writer
        .replay(sid)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r, RolloutRecord::Message { .. }))
        .collect();
    assert_eq!(messages, expected);
}
