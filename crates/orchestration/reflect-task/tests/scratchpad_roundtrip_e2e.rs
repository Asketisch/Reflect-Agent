//! scratchpad WriteNote + ReadNotes 端到端 roundtrip。

use std::sync::Arc;

use reflect_task::tools::{ReadNotesTool, WriteNoteTool};
use reflect_tools::{Tool, ToolContext};
use tempfile::TempDir;

#[tokio::test]
async fn write_then_read_note_roundtrip() {
    let dir = TempDir::new().unwrap();
    let root = Arc::new(dir.path().to_path_buf());
    let write = WriteNoteTool::new(root.clone());
    let read = ReadNotesTool::new(root);

    write
        .execute(
            ToolContext::default(),
            serde_json::json!({"name": "findings", "body": "hello scratchpad"}),
        )
        .await
        .unwrap();

    let out = read
        .execute(
            ToolContext::default(),
            serde_json::json!({"name": "findings"}),
        )
        .await
        .unwrap();
    let text = out.content[0].as_text().unwrap();
    assert!(text.contains("hello scratchpad"));
}

#[tokio::test]
async fn read_notes_lists_all_sorted() {
    let dir = TempDir::new().unwrap();
    let root = Arc::new(dir.path().to_path_buf());
    let write = WriteNoteTool::new(root.clone());
    let read = ReadNotesTool::new(root);

    write
        .execute(
            ToolContext::default(),
            serde_json::json!({"name": "zebra", "body": "z"}),
        )
        .await
        .unwrap();
    write
        .execute(
            ToolContext::default(),
            serde_json::json!({"name": "alpha", "body": "a"}),
        )
        .await
        .unwrap();

    let out = read
        .execute(ToolContext::default(), serde_json::json!({}))
        .await
        .unwrap();
    let text = out.content[0].as_text().unwrap();
    let alpha_pos = text.find("alpha").expect("alpha");
    let zebra_pos = text.find("zebra").expect("zebra");
    assert!(alpha_pos < zebra_pos);
}

trait ContentBlockText {
    fn as_text(&self) -> Option<&str>;
}

impl ContentBlockText for reflect_protocol::ContentBlock {
    fn as_text(&self) -> Option<&str> {
        match self {
            reflect_protocol::ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }
}
