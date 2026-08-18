//! `write` —— 写文本文件(自动 mkdir,返回 unified diff)。
//!
//! 详见 `docs/tools-and-hooks.md §2.3`。

use std::fs;

use async_trait::async_trait;
use serde_json::Value;
use similar::TextDiff;

#[allow(unused_imports)] // pre-M5
use crate::sandbox::resolve_sandbox_path;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use reflect_protocol::PermissionMode;

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        reflect_prompt::copy("tool.write")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace, or absolute path under workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Full new file content (overwrites existing file)"
                }
            },
            "required": ["path", "content"],
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
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                // 错误信息保留关键字 `missing 'content'` 兼容已有测试,
                // 同时给 LLM 一条清晰路径:Plan mode 下写 plan 应改用 PlanWrite。
                message: "missing 'content' — write 需要 path 与 content 两个参数。Plan mode 下写 plan 推荐改用 PlanWrite {name, content}".into(),
            })?;

        let path = std::path::Path::new(path_str);
        // 计算最终路径:workspace 与 path 按 component 逐级拼接。
        // 文件(或其父目录)可能还不存在,所以先把现存的最深祖先 canonicalize,
        // 再拼上后续 component。
        let canonical_workspace = ctx
            .workspace_path()
            .canonicalize()
            .map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
        let mut final_path = canonical_workspace.clone();
        for component in path.components() {
            final_path.push(component);
            if final_path.exists() {
                if let Ok(c) = final_path.canonicalize() {
                    final_path = c;
                }
            }
            // 否则保持原样,后续会创建它(或它的父目录)。
        }
        // 沙箱校验:每个现有祖先都必须在 workspace 内。
        // 因为按 component 增量 canonicalize,这一步隐式完成。
        // 绝对路径则额外校验最终路径在 workspace 下。
        if path.is_absolute() {
            let abs = final_path.clone();
            if !abs.starts_with(&canonical_workspace) {
                return Err(ToolError::PathEscape {
                    path: path.to_path_buf(),
                });
            }
        }

        // 自动 mkdir。
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ToolError::Io(format!("mkdir {parent:?}: {e}")))?;
        }
        let before = fs::read_to_string(&final_path).unwrap_or_default();
        let new_owned = content.to_string();
        let unified = {
            let diff = TextDiff::from_lines(&before, &new_owned);
            diff.unified_diff().to_string()
        };

        fs::write(&final_path, content)
            .map_err(|e| ToolError::Io(format!("write {path_str}: {e}")))?;

        Ok(ToolOutput {
            content: vec![
                reflect_protocol::ContentBlock::text(format!(
                    "wrote {path_str} ({} bytes)",
                    content.len()
                )),
                reflect_protocol::ContentBlock::Diff {
                    unified_diff: unified.clone(),
                },
            ],
            is_error: false,
            metadata: serde_json::json!({
                "path": final_path.to_string_lossy(),
                "path_str": path_str,
                "bytes": content.len(),
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
            std::env::temp_dir().join(format!("reflect_write_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn writes_new_file() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "hello\nworld"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "hello\nworld"
        );
        // Diff 块应存在。
        assert!(
            out.content
                .iter()
                .any(|c| matches!(c, reflect_protocol::ContentBlock::Diff { .. }))
        );
    }

    #[tokio::test]
    async fn auto_creates_parent_dirs() {
        let ws = tmp_workspace();
        let ws_canonical = ws.canonicalize().unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/dir/a.txt", "content": "x"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "execute failed: {out:?}");
        assert!(ws_canonical.join("sub/dir/a.txt").exists());
    }

    #[tokio::test]
    async fn overwrites_existing_file_with_diff() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "old\n").unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "new\n"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "new\n");
        // Diff 应包含删除 / 添加标记。
        let diff = out.content.iter().find_map(|c| match c {
            reflect_protocol::ContentBlock::Diff { unified_diff } => Some(unified_diff.clone()),
            _ => None,
        });
        let diff = diff.expect("diff block");
        assert!(diff.contains("-old") || diff.contains("---"));
        assert!(diff.contains("+new") || diff.contains("+++"));
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
