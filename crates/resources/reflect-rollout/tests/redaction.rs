//! 端到端测试:写入一条带 20KB content 字段的记录,回读文件,
//! 确认该字段被截断并附加 `[redacted]` 标记。

use reflect_protocol::{MessageRole, RolloutRecord, RolloutRecorder, ThreadId, TurnId};
use reflect_rollout::JsonlRolloutWriter;
use tempfile::tempdir;

#[tokio::test]
async fn large_string_field_is_truncated_on_disk() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);

    let big = "y".repeat(20_000);
    writer
        .record(RolloutRecord::message(
            TurnId::new(),
            MessageRole::Assistant,
            serde_json::json!(big),
        ))
        .await
        .unwrap();

    let path = find_first_jsonl(dir.path()).expect("expected a jsonl file");
    let body = std::fs::read_to_string(&path).unwrap();

    assert!(
        body.contains("[redacted]"),
        "expected [redacted] marker in {body}"
    );
    // 正文大小应远小于原始 20KB。
    assert!(body.len() < 18_000, "body too large: {} bytes", body.len());
}

#[tokio::test]
async fn small_fields_pass_through_untouched() {
    let dir = tempdir().unwrap();
    let sid = ThreadId::new();
    let writer = JsonlRolloutWriter::new(dir.path(), sid);
    writer
        .record(RolloutRecord::session_meta(sid, "openai/gpt-4o"))
        .await
        .unwrap();
    writer
        .record(RolloutRecord::message(
            TurnId::new(),
            MessageRole::User,
            serde_json::json!("hi"),
        ))
        .await
        .unwrap();
    let path = find_first_jsonl(dir.path()).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(!body.contains("[redacted]"));
    assert!(body.contains("\"model\":\"openai/gpt-4o\""));
    assert!(body.contains("\"content\":\"hi\""));
}

fn find_first_jsonl(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut out = None;
    fn walk(p: &std::path::Path, out: &mut Option<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if out.is_none()
                    && path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                {
                    *out = Some(path);
                }
            }
        }
    }
    walk(root, &mut out);
    out
}
