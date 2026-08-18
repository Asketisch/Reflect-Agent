//! `AddSessionNoteTool` — LLM 主动追加 session memory note 的入口。
//!
//! 完全照抄 `reflect_tools::builtins::bash::BashTool` 的模板:
//! 唯一参数 `text: String`,幂等 + 低风险 → `required_permission = Auto`,
//! 每次调 `NoteStore::add()` FIFO 推入 + JSONL 追加。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::PermissionMode;
use reflect_tools::{Tool, ToolContext};

use crate::store::NoteStore;

/// `add_session_note` 工具实现。
///
/// 暴露给 LLM 后,每轮可主动调一次或多次,`pre_loop` 在下一轮把
/// `NoteStore::as_meta_message()` 注入到 `<system-reminder>`。配合
/// FIFO 30 上限,旧 note 自动淘汰,不会撑爆 context。
pub struct AddSessionNoteTool {
    store: Arc<dyn NoteStore>,
}

impl std::fmt::Debug for AddSessionNoteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddSessionNoteTool")
            .field("store", &"<dyn NoteStore>")
            .finish()
    }
}

impl AddSessionNoteTool {
    pub fn new(store: Arc<dyn NoteStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for AddSessionNoteTool {
    fn name(&self) -> &str {
        "add_session_note"
    }

    fn description(&self) -> &str {
        "向当前 session 追加一条简短 memory note(跨 turn 持久保留关键事实、\
         用户偏好或中间结论)。Note 会在后续 turn 自动注入回你的上下文。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "要保存的 note 内容(中英文均可,推荐简洁)。前后空白会被自动 trim。",
                    "minLength": 1,
                    "maxLength": 4096,
                }
            },
            "required": ["text"],
            "additionalProperties": false,
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 纯内存 + append-only 写,无外部副作用。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        // 幂等,只能改自己的 session note 队列,无文件系统副作用(除
        // JSONL 追加)。`Auto` 让 LLM 自由使用,不阻塞思考。
        PermissionMode::Auto
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<reflect_protocol::ToolOutput, reflect_protocol::ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| reflect_protocol::ToolError::InvalidArgs {
                message: "missing 'text' string".into(),
            })?
            .to_string();

        let note = self.store.add(text).map_err(|e| match e {
            crate::NoteError::Empty => reflect_protocol::ToolError::InvalidArgs {
                message: "text is empty after trim".into(),
            },
            crate::NoteError::TooLong(n) => reflect_protocol::ToolError::InvalidArgs {
                message: format!("text too long: {n} chars (max 4096)"),
            },
            other => reflect_protocol::ToolError::Execution(other.to_string()),
        })?;

        let count = self.store.list().len();
        let summary = format!(
            "Note recorded at {} (total: {}, cap: {})",
            note.created_at.format("%Y-%m-%dT%H:%M:%SZ"),
            count,
            crate::store::SESSION_NOTE_CAP,
        );

        Ok(reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(summary)],
            is_error: false,
            metadata: serde_json::json!({
                "note_count": count,
                "cap": crate::store::SESSION_NOTE_CAP,
                "note_created_at": note.created_at.to_rfc3339(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryNoteStore;

    fn ctx() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn add_then_list_roundtrip() {
        let store: Arc<dyn NoteStore> = Arc::new(InMemoryNoteStore::new());
        let t = AddSessionNoteTool::new(store.clone());
        let out = t
            .execute(ctx(), serde_json::json!({"text": "hello"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        let meta = out.metadata;
        assert_eq!(meta["note_count"], 1);
        assert_eq!(store.list().len(), 1);
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let store: Arc<dyn NoteStore> = Arc::new(InMemoryNoteStore::new());
        let t = AddSessionNoteTool::new(store);
        let err = t
            .execute(ctx(), serde_json::json!({"text": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            reflect_protocol::ToolError::InvalidArgs { .. }
        ));
    }

    #[tokio::test]
    async fn rejects_missing_text() {
        let store: Arc<dyn NoteStore> = Arc::new(InMemoryNoteStore::new());
        let t = AddSessionNoteTool::new(store);
        let err = t.execute(ctx(), serde_json::json!({})).await.unwrap_err();
        assert!(matches!(
            err,
            reflect_protocol::ToolError::InvalidArgs { .. }
        ));
    }

    #[tokio::test]
    async fn name_and_schema_match_spec() {
        let store: Arc<dyn NoteStore> = Arc::new(InMemoryNoteStore::new());
        let t = AddSessionNoteTool::new(store);
        assert_eq!(t.name(), "add_session_note");
        let s = t.parameters_schema();
        assert_eq!(s["required"][0], "text");
    }

    #[tokio::test]
    async fn is_concurrency_safe_and_auto_permission() {
        let store: Arc<dyn NoteStore> = Arc::new(InMemoryNoteStore::new());
        let t = AddSessionNoteTool::new(store);
        assert!(t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Auto);
    }
}
