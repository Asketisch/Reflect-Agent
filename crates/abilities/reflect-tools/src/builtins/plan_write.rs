//! `PlanWrite` —— Plan mode 专用写盘工具。
//!
//! agent 在 Plan mode 调研完成后,把完整 plan markdown 写入
//! `<workspace>/.reflect/plan/<name>.md`。`ExitPlanMode` 调用时由
//! `tool_exec::read_latest_plan_file` 扫描该目录取最新 mtime 文件读取,
//! 所以 `PlanWrite` 写进这个目录就自动被 `ExitPlanMode` 选中。
//!
//! ## 为什么需要独立工具(而非复用 `write` + `.reflect/plan/` 路径特例)
//!
//! 此前 Plan mode 写 plan 走通用 `write` 工具,依赖 `PlanModeGate` 里
//! `path.contains(".reflect/plan/")` 的字符串特例放行 hook,但审批层
//! (`ToolExecutionQueue`)只看 `WriteTool::required_permission() == Prompt`,
//! 与 Plan mode 无关 —— 所以每次写 plan 仍弹「⚠ Approved: write」。
//! 而且字符串匹配无路径规范化,可被 `..` / 符号链接绕过。
//!
//! `PlanWrite` 一次性解决:
//! - `required_permission() == Auto` → 审批层 `tool_requires_prompt = false`,
//!   Plan mode 下直接落盘、不弹窗。
//! - 进 `PlanModeGate` 白名单 → hook 直放。
//! - 内部 canonicalize 后强制断言落在 `<workspace>/.reflect/plan/` 之下,
//!   拒绝 `..` / 绝对路径 / 非 `.md` 扩展名。
//!
//! 设计要点:
//! - `is_concurrency_safe = false`(有文件写副作用;虽然 `write.rs` 也是
//!   `false`,这里保持一致,避免并发写同一 plan 文件)
//! - `required_permission = Auto`(免审批的关键,见上)
//! - `path` 参数只接受文件名或相对子路径;工具内部拼到 `.reflect/plan/`
//!   下,调用方无需感知目录约定。

use std::fs;
use std::path::{Component, Path};

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolError, ToolOutput};
use serde_json::Value;

use crate::tool::{Tool, ToolContext};

/// Plan 文档落盘根目录(workspace 下的相对路径)。
const PLAN_DIR_REL: &str = ".reflect/plan";

pub struct PlanWriteTool;

#[async_trait]
impl Tool for PlanWriteTool {
    fn name(&self) -> &str {
        "PlanWrite"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.PlanWrite")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Plan 文件名或相对路径(如 `my-plan.md`)。工具自动落到 workspace 的 `.reflect/plan/` 下;不接受绝对路径或 `..`。"
                },
                "content": {
                    "type": "string",
                    "description": "完整 plan markdown 内容。"
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
        // Auto → 审批层 tool_requires_prompt = false,Plan mode 下不弹窗。
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'path'".into(),
            })?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'content'".into(),
            })?;

        let rel = Path::new(path_str);

        // 拒绝绝对路径与任何 parent/`..` 段 —— 这些是路径逃逸的必要条件。
        // (normalize 在 join 之后还会用 canonicalize + strip_prefix 再校验一次。)
        if rel.is_absolute() {
            return Err(ToolError::PathEscape {
                path: rel.to_path_buf(),
            });
        }
        if rel.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ToolError::PathEscape {
                path: rel.to_path_buf(),
            });
        }

        // 强制 `.md` 扩展名,与 `read_latest_plan_file` 的扫描过滤对齐
        // (它只读 `.md`)。
        if rel.extension().and_then(|e| e.to_str()) != Some("md") {
            return Err(ToolError::InvalidArgs {
                message: format!("plan path must end in `.md`: {path_str}"),
            });
        }

        // workspace 可能在会话中被热切换,取当前快照。workspace 根必须真实
        // 存在(才能 canonicalize);否则报 io 错而非 panic。
        let canonical_workspace = ctx
            .workspace_path()
            .canonicalize()
            .map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
        let plan_dir = canonical_workspace.join(PLAN_DIR_REL);

        // 幂等创建 plan 目录。
        fs::create_dir_all(&plan_dir)
            .map_err(|e| ToolError::Io(format!("mkdir {}: {e}", plan_dir.display())))?;

        // 拼接最终路径。`rel` 已剔除 `..` 与绝对路径,这里用逐 component
        // 拼接(而非 `plan_dir.join(path_str)` 字符串拼接)以保留规范化语义。
        let mut final_path = plan_dir.clone();
        for component in rel.components() {
            final_path.push(component);
        }
        // 嵌套相对路径(如 `sub/p.md`)需要先创建其父目录。
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ToolError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }

        // 二次校验:canonicalize 已存在部分 + 最终落点必须在 plan_dir 之内。
        // 逐 component 推进,对已存在的中间段 canonicalize,堵住符号链接逃逸。
        let canonical_plan_dir = plan_dir.canonicalize().unwrap_or_else(|_| plan_dir.clone());
        let mut probe = canonical_plan_dir.clone();
        for component in rel.components() {
            probe.push(component);
            if probe.exists() {
                if let Ok(c) = probe.canonicalize() {
                    probe = c;
                }
            }
        }
        if !probe.starts_with(&canonical_plan_dir) {
            return Err(ToolError::PathEscape {
                path: rel.to_path_buf(),
            });
        }

        // 写盘(final_path 此时可能含未规范化的最后一段 —— 那正是要创建的文件,
        // 直接写即可;parent 已由 create_dir_all 保证存在)。
        fs::write(&final_path, content)
            .map_err(|e| ToolError::Io(format!("write {}: {e}", final_path.display())))?;

        // 返回写入路径(规范化后的 plan_dir 前缀 + 相对段),供观测/ExitPlanMode
        // 链路确认。`read_latest_plan_file` 会按 mtime 重新扫描,不依赖此值。
        let written_display = canonical_plan_dir.join(rel);
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "wrote plan to {} ({} bytes)",
                written_display.display(),
                content.len()
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "path": written_display.to_string_lossy(),
                "bytes": content.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_workspace() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "reflect_planwrite_test_{}_{}",
            std::process::id(),
            n
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(ws: &Path) -> ToolContext {
        ToolContext::for_workspace(ws)
    }

    #[tokio::test]
    async fn writes_plan_to_reflect_plan_dir() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let out = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "my-plan.md", "content": "# Plan\n1. step"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let written = ws.canonicalize().unwrap().join(".reflect/plan/my-plan.md");
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "# Plan\n1. step"
        );
    }

    #[tokio::test]
    async fn auto_creates_plan_dir() {
        // 目录初始不存在。
        let ws = tmp_workspace();
        assert!(!ws.join(".reflect/plan").exists());
        let t = PlanWriteTool;
        let out = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "p.md", "content": "x"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(ws.join(".reflect/plan").exists());
    }

    #[tokio::test]
    async fn supports_nested_relative_path() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let out = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "sub/p.md", "content": "y"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(ws.join(".reflect/plan/sub/p.md").exists());
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let err = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "/etc/evil.md", "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }));
    }

    #[tokio::test]
    async fn rejects_parent_dir_traversal() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let err = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "../escape.md", "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }));
    }

    #[tokio::test]
    async fn rejects_non_md_extension() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let err = t
            .execute(
                ctx(&ws),
                serde_json::json!({"path": "plan.txt", "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let err = t
            .execute(ctx(&ws), serde_json::json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_missing_content() {
        let ws = tmp_workspace();
        let t = PlanWriteTool;
        let err = t
            .execute(ctx(&ws), serde_json::json!({"path": "p.md"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let t = PlanWriteTool;
        assert_eq!(t.name(), "PlanWrite");
        assert!(!t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Auto);
    }
}
