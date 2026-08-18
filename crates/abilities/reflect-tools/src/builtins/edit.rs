//! `edit` — find-and-replace edit (unique match required).
//!
//! See `docs/tools-and-hooks.md §2.4`.

use std::fs;

use async_trait::async_trait;
use serde_json::Value;
use similar::TextDiff;

use crate::sandbox::resolve_sandbox_path;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use reflect_protocol::PermissionMode;

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        reflect_prompt::copy("tool.edit")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace, or absolute path under workspace"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find (must match exactly once in the file; 0 or >1 matches fail)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text (use empty string to delete)"
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'old_string'".into(),
            })?;
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'new_string'".into(),
            })?;

        let path = std::path::Path::new(path_str);
        let abs = resolve_sandbox_path(&ctx.workspace_path(), path)?;
        let before =
            fs::read_to_string(&abs).map_err(|e| ToolError::Io(format!("read {path_str}: {e}")))?;

        let occurrences = before.matches(old).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidArgs {
                message: format!("old_string not found in {path_str}"),
            });
        }
        if occurrences > 1 {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "old_string matches {occurrences} times in {path_str}; must be unique"
                ),
            });
        }

        let after = before.replacen(old, new, 1);
        let diff = TextDiff::from_lines(&before, &after);
        let unified = format!("{}", diff.unified_diff());

        fs::write(&abs, &after).map_err(|e| ToolError::Io(format!("write {path_str}: {e}")))?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Diff {
                unified_diff: unified.clone(),
            }],
            is_error: false,
            metadata: serde_json::json!({
                "path": abs.to_string_lossy(),
                "path_str": path_str,
                "diff": unified,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_ctx(workspace: &std::path::Path) -> ToolContext {
        ToolContext::for_workspace(workspace)
    }

    fn tmp_workspace() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("reflect_edit_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn replaces_unique_string() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "alpha\nbeta\ngamma").unwrap();
        let t = EditTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "alpha\nBETA\ngamma"
        );
    }

    #[tokio::test]
    async fn rejects_zero_matches() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "alpha\nbeta").unwrap();
        let t = EditTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "old_string": "missing", "new_string": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_multiple_matches() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "foo\nfoo\nbar").unwrap();
        let t = EditTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "x"}),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { message } => assert!(message.contains("2 times")),
            other => panic!("expected InvalidArgs with count, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_missing_args() {
        let ws = tmp_workspace();
        let t = EditTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"path": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
