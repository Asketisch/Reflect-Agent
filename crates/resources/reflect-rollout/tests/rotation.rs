//! 端到端测试:写足够多的记录触发轮转,然后验证轮转文件存在,且当前文件缩回小体积。

use reflect_protocol::{MessageRole, RolloutRecord, RolloutRecorder, ThreadId, TurnId};
use reflect_rollout::JsonlRolloutWriter;
use tempfile::tempdir;

#[tokio::test]
async fn rotates_at_256kb_and_keeps_at_most_3_copies() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);

    // 每条记录约 2 KiB;200 条 => ~400 KiB => 至少触发 1 次轮转。
    for i in 0..200 {
        writer
            .record(RolloutRecord::message(
                TurnId::new(),
                MessageRole::Assistant,
                serde_json::json!({"i": i, "blob": "x".repeat(2048)}),
            ))
            .await
            .unwrap();
    }

    // 遍历日期目录检查:至少出现一个 `.1.jsonl`,绝不出现 `.4.jsonl`。
    let mut found_one = false;
    let mut found_four = false;
    walk(dir.path(), &mut |path| {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".1.jsonl") {
            found_one = true;
        }
        if name.ends_with(".4.jsonl") {
            found_four = true;
        }
    });
    assert!(found_one, "应存在 .1.jsonl 轮转文件");
    assert!(!found_four, "不应出现 .4.jsonl");
}

#[tokio::test]
async fn current_file_is_small_after_rotation() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);

    // 强制多次轮转。
    for i in 0..(5 * 200) {
        writer
            .record(RolloutRecord::message(
                TurnId::new(),
                MessageRole::Assistant,
                serde_json::json!({"i": i, "blob": "x".repeat(2048)}),
            ))
            .await
            .unwrap();
    }

    // 当前(未轮转)文件应远小于 256 KiB。
    let mut sizes = Vec::new();
    let sid_str = sid.to_string();
    walk(dir.path(), &mut |path| {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // 当前文件是 `<sid>.jsonl`;轮转文件是 `<sid>.<n>.jsonl`。
        if name == format!("{sid_str}.jsonl") {
            sizes.push(std::fs::metadata(path).unwrap().len());
        }
    });
    assert!(!sizes.is_empty(), "应存在当前文件");
    let cur = *sizes.iter().max().unwrap();
    assert!(cur < 256 * 1024, "轮转后当前文件应 < 256 KiB,实际 {cur}");
}

fn walk<F: FnMut(&std::path::Path)>(root: &std::path::Path, f: &mut F) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, f);
            } else {
                f(&p);
            }
        }
    }
}
