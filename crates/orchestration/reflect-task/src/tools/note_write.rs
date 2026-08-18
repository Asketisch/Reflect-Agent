//! `WriteNote` —— 把一段文本写入 coordinator scratchpad(`<scratchpad>/<name>.md`)。
//!
//! 参数:`name`(kebab-case,≤ 64 chars)、`body`(≤ 16 KiB,append 模式)。
//!
//! 行为:把 body 追加到 `<scratchpad_root>/<name>.md`(若文件不存在则创建)。
//! 用 `tokio::fs::OpenOptions::append + create` 写,失败转 `ToolError::Io`。
//!
//! 权限:`Auto`,因为 scratchpad 是 session-private 临时目录,默认允许
//! worker 写入。Coordinator 自身在 v1.1.0 通过 ReadNotes 读 worker 写的 note,
//! 不需要自己的 WriteNote 调用面(留 v1.2 加权限分级)。
//!
//! scratchpad_root 是 `None`(coordinator 模式未启用)时,工具返
//! `ToolError::InvalidArgs`,与 `validate_note_name` 失败的语义一致。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;
use tracing::warn;

use crate::coordinator::{MAX_NOTE_BODY_BYTES, validate_note_name};
use crate::tools::parse_string_arg;

/// `WriteNote` 工具实现。
pub struct WriteNoteTool {
    scratchpad_root: Arc<PathBuf>,
}

impl std::fmt::Debug for WriteNoteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteNoteTool")
            .field(
                "scratchpad_root",
                &self.scratchpad_root.display().to_string(),
            )
            .finish()
    }
}

impl WriteNoteTool {
    pub fn new(scratchpad_root: Arc<PathBuf>) -> Self {
        Self { scratchpad_root }
    }
}

#[async_trait]
impl Tool for WriteNoteTool {
    fn name(&self) -> &str {
        "WriteNote"
    }

    fn description(&self) -> &str {
        "把一段文本 append 到 coordinator scratchpad 下的 <name>.md 文件。\
         name 必须 kebab-case(小写字母/数字/-/_),body 限制 16 KiB,\
         超出会被截断并 metadata.truncated=true。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "pattern": "^[a-z0-9_-]+$",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Note 文件名(不含 .md 后缀),kebab-case"
                },
                "body": {
                    "type": "string",
                    "description": "要追加的文本内容,UTF-8"
                }
            },
            "required": ["name", "body"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // append 是单文件单 writer 模式,多次调用顺序敏感,标 false。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let name = parse_string_arg(&args, "name")?;
        validate_note_name(&name).map_err(|msg| ToolError::InvalidArgs { message: msg })?;
        let body = parse_string_arg(&args, "body")?;

        let path = self.scratchpad_root.join(format!("{name}.md"));

        // 截断 body 至 MAX_NOTE_BODY_BYTES 字节(UTF-8 安全:按 char 取截断)。
        let (truncated_body, truncated) = if body.len() > MAX_NOTE_BODY_BYTES {
            // 按字节找最近的 char boundary。
            let mut idx = MAX_NOTE_BODY_BYTES;
            while !body.is_char_boundary(idx) && idx > 0 {
                idx -= 1;
            }
            (body[..idx].to_string(), true)
        } else {
            (body, false)
        };

        // append 模式写入。
        let path_for_op = path.clone();
        let bytes = truncated_body.as_bytes().to_vec();
        let bytes_len = bytes.len();
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            if let Some(parent) = path_for_op.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_for_op)?;
            f.write_all(&bytes)?;
            // append 之后追加换行,便于多次写入分隔。
            f.write_all(b"\n")?;
            Ok(())
        })
        .await
        .map_err(|e| ToolError::Execution(format!("spawn_blocking join: {e}")))?;
        if let Err(e) = result {
            warn!(path = %path.display(), error = %e, "WriteNote failed");
            return Err(ToolError::Io(e.to_string()));
        }

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Wrote {} bytes to {}",
                bytes_len,
                path.display()
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "name": name,
                "path": path.display().to_string(),
                "bytesWritten": bytes_len,
                "truncated": truncated,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let tool = WriteNoteTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "findings", "body": "module X uses Y"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["name"], "findings");
        assert_eq!(out.metadata["truncated"], false);
        let content = std::fs::read_to_string(dir.path().join("findings.md")).unwrap();
        assert!(content.contains("module X uses Y"));
        // append 模式自动加换行
        assert!(content.ends_with('\n'));
    }

    #[tokio::test]
    async fn write_appends_multiple_calls() {
        let dir = TempDir::new().unwrap();
        let tool = WriteNoteTool::new(Arc::new(dir.path().to_path_buf()));
        for body in ["first", "second", "third"] {
            tool.execute(
                ToolContext::default(),
                serde_json::json!({"name": "log", "body": body}),
            )
            .await
            .unwrap();
        }
        let content = std::fs::read_to_string(dir.path().join("log.md")).unwrap();
        assert!(content.contains("first"));
        assert!(content.contains("second"));
        assert!(content.contains("third"));
    }

    #[tokio::test]
    async fn write_rejects_invalid_name() {
        let dir = TempDir::new().unwrap();
        let tool = WriteNoteTool::new(Arc::new(dir.path().to_path_buf()));
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "Bad Name!", "body": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn write_truncates_oversized_body() {
        let dir = TempDir::new().unwrap();
        let tool = WriteNoteTool::new(Arc::new(dir.path().to_path_buf()));
        let big = "x".repeat(MAX_NOTE_BODY_BYTES + 100);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "big", "body": big}),
            )
            .await
            .unwrap();
        assert!(out.metadata["truncated"] == true);
        assert!(out.metadata["bytesWritten"].as_u64().unwrap() <= MAX_NOTE_BODY_BYTES as u64);
        let content = std::fs::read_to_string(dir.path().join("big.md")).unwrap();
        assert!(content.len() <= MAX_NOTE_BODY_BYTES + 1); // +1 for newline
    }

    #[tokio::test]
    async fn write_requires_name_and_body() {
        let dir = TempDir::new().unwrap();
        let tool = WriteNoteTool::new(Arc::new(dir.path().to_path_buf()));
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"body": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let tool = WriteNoteTool::new(Arc::new(PathBuf::from("/tmp/x")));
        assert_eq!(tool.name(), "WriteNote");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
        assert!(required.iter().any(|v| v == "body"));
    }
}
