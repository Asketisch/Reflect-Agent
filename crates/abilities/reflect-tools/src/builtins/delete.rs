//! `delete_file` — 删除 workspace 内单个文件。
//!
//! ## 设计要点
//!
//! - **权限模型**:`required_permission = PermissionMode::Prompt` —— 删除
//!   不可逆,由 `ToolExecutionQueue` 触发 `ApprovalGate` 模态框让用户确认。
//! - **沙箱**:`resolve_sandbox_path` 先 canonicalize + 校验 in-workspace,
//!   转义路径返回 `ToolError::PathEscape`,与 read/write/edit 共用同一基线。
//! - **不加入 `WRITE_TOOLS` 白名单**:`ReadBeforeEditHook` 不拦截
//!   `delete_file` —— 删除不可逆,**不依赖文件内容知识**(`Prompt` 权限门
//!   兜底),后续 forget 不必显式通知 hook(文件消失后
//!   `FileReadStateTracker` 中的 stale entry 自然失效)。详见
//!   `reflect_hooks::read_before_edit::WRITE_TOOLS` 注释。
//! - **错误类型**:`InvalidArgs` 表示用户输入错(空路径 / 目录目标 / 文件
//!   不存在);`Io` 表示真实 IO 失败(权限不足 / 设备错误等),按
//!   `io::ErrorKind` 给出 hint。
//!
//! ## TOCTOU
//!
//! `metadata` 预检与 `resolve_sandbox_path` 之间,以及后者与 `remove_file`
//! 之间存在 TOCTOU 窗口。对单用户 workspace 可接受 —— 唯一竞争者是 agent
//! 自身(并发 write/edit 同一路径),已被 `Prompt` 权限门 + `cancel` token
//! 兜底。多租户文件系统隔离不在 v1 假设内。

use std::fs;
use std::io;

use async_trait::async_trait;
use serde_json::Value;
use tracing::field;

use crate::sandbox::resolve_sandbox_path;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use reflect_protocol::PermissionMode;

pub struct DeleteTool;

#[async_trait]
impl Tool for DeleteTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.delete_file")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative or absolute path to the file"}
            },
            "required": ["path"],
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
        // ── tracing span ────────────────────────────────────────────────
        // 字段 path/path_str/outcome 在关键节点 record,便于事后追查
        // "agent 究竟删了哪个文件"。与 web_fetch / web_search / ask_user
        // 的 span 模式一致。
        let span = tracing::info_span!(
            "delete_file.execute",
            path = field::Empty,
            path_str = field::Empty,
            outcome = field::Empty,
        );
        let _enter = span.enter();

        // ── 参数解析 + 基础校验 ────────────────────────────────────────
        let raw_path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        let path_str = raw_path_str.trim();
        if path_str.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "path must not be empty".into(),
            });
        }
        if args.as_object().map(|o| o.len()).unwrap_or(0) != 1 {
            return Err(ToolError::InvalidArgs {
                message: "delete_file only accepts the 'path' field".into(),
            });
        }
        span.record("path_str", path_str);

        let raw_path = std::path::Path::new(path_str);
        let in_workspace = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            ctx.workspace_path().join(raw_path)
        };

        // ── 预检:目录 / 不存在 / 权限 ─────────────────────────────────
        // 用未 canonicalize 的路径做 `metadata` —— `metadata` 跟随 symlink
        // 并解析 `..`,足以区分"目录 vs 文件 vs 不存在"。这里没有沙箱
        // 风险(我们只读 metadata,不暴露内容),真正的 in-workspace 校验
        // 由下面的 `resolve_sandbox_path` 兜底。
        match fs::metadata(&in_workspace) {
            Ok(m) if m.is_dir() => {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "delete_file only supports files, not directories: {path_str}"
                    ),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ToolError::InvalidArgs {
                    message: format!("file not found: {path_str}"),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(ToolError::Io(format!(
                    "permission denied inspecting {path_str}: {e}"
                )));
            }
            _ => {} // 文件存在且非目录 / 其他 IO 错误留给后面 `remove_file` 兜底
        }

        // ── 沙箱校验:canonicalize + in-workspace ──────────────────────
        // 此处失败一律 `Io`(workspace 不可达)或 `PathEscape`(转义)。
        let final_path = resolve_sandbox_path(&ctx.workspace_path(), raw_path)?;
        span.record("path", final_path.to_string_lossy().as_ref());

        // ── 删除 ──────────────────────────────────────────────────────
        // SECURITY:canonicalize(沙箱)与 remove_file 之间存在 TOCTOU 窗口
        // —— 单用户 workspace 可接受,见模块头注释。
        fs::remove_file(&final_path).map_err(|e| {
            let hint = match e.kind() {
                io::ErrorKind::PermissionDenied => " (permission denied — check file mode/owner)",
                io::ErrorKind::NotFound => {
                    " (file disappeared between check and delete — concurrent mutation)"
                }
                io::ErrorKind::TooManyLinks => " (too many hard links)",
                io::ErrorKind::CrossesDevices => " (cross-device link)",
                _ => "",
            };
            ToolError::Io(format!("delete {path_str}: {e}{hint}"))
        })?;

        span.record("outcome", "deleted");

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "deleted {path_str}"
            ))],
            is_error: false,
            metadata: serde_json::json!({
                // canonical 绝对路径 —— 与 read.rs 对齐,便于 hook / trace
                // 端按 path 关联。
                // 注意:delete 不写 `mtime_ms`,因为 (1) 删除不读文件内容,
                // (2) FileReadStateTracker 中的 stale entry 在文件消失后
                // 自然失效,无需显式 forget(详见模块头"不加入 WRITE_TOOLS
                // 白名单"段)。
                "path": final_path.to_string_lossy(),
                "path_str": path_str,
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
    async fn deletes_existing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("to_delete.txt");
        std::fs::write(&file, "bye").unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let out = tool
            .execute(ctx, serde_json::json!({"path": "to_delete.txt"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(!file.exists());
    }

    /// 不存在文件 → `InvalidArgs`(契约:用户输入错误,不是 IO 失败)。
    /// 修复前返回 `ToolError::Io("No such file or directory")`,无法区分
    /// "用户路径写错"与"系统 IO 失败"。
    #[tokio::test]
    async fn missing_file_errors_with_invalid_args() {
        let dir = TempDir::new().unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let err = tool
            .execute(ctx, serde_json::json!({"path": "nope.txt"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { .. }),
            "缺失文件应返回 InvalidArgs,got {err:?}"
        );
    }

    #[tokio::test]
    async fn directory_target_is_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let err = tool
            .execute(ctx, serde_json::json!({"path": "subdir"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let err = tool
            .execute(ctx, serde_json::json!({"path": "  "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn missing_path_field_is_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let err = tool.execute(ctx, serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    /// extra fields → InvalidArgs(schema 声明 additionalProperties: false)。
    #[tokio::test]
    async fn extra_fields_are_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = DeleteTool;
        let ctx = ToolContext::for_workspace(dir.path());
        let err = tool
            .execute(ctx, serde_json::json!({"path": "x.txt", "force": true}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
