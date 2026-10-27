//! `glob` —— 查找匹配 glob 模式的文件。
//!
//! 详见 `docs/tools-and-hooks.md §2.6`。M3 v0 用 `ignore::WalkBuilder`
//! 遍历 workspace;`pattern` 在相对路径上做子串匹配(v0 阶段为节省成本,
//! 完整的 globset 匹配推迟到 v1)。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_RESULTS: usize = 1000;
const IGNORE_DIRS: &[&str] = &["target", "node_modules", ".git"];

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.glob")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Substring or simple glob to match (e.g. '*.rs', 'src/main')"},
                "path": {"type": "string", "description": "Search root (default: workspace)"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'pattern'".into(),
            })?;
        let root: PathBuf = match args.get("path").and_then(|v| v.as_str()) {
            // 0.6:`path` 此前未做 workspace 包含校验,直接喂给
            // `WalkBuilder::new(root)` —— 与 grep 同样的信息泄露面。
            Some(p) => crate::sandbox::resolve_sandbox_dir(&ctx.workspace_path(), Path::new(p))?,
            None => ctx.workspace_path(),
        };

        // 剥掉开头的 "*." 用作后缀匹配。
        let suffix = pattern.trim_start_matches("*.");

        let mut paths: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        let walker = WalkBuilder::new(&root)
            .standard_filters(true)
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !IGNORE_DIRS.iter().any(|d| name == *d)
            })
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let p = entry.path();
            let rel = p.strip_prefix(&root).unwrap_or(p);
            let rel_str = rel.to_string_lossy();
            // 注:原实现这里有个 `if pattern.contains('/')` 分支,两个臂
            // 逐字相同(`rel_str.contains(pattern)`)—— 纯死代码。已删除,
            // 匹配逻辑与是否含 `/` 无关。
            let matches = rel_str.contains(pattern)
                || (!suffix.is_empty() && p.extension().is_some_and(|e| e == suffix));
            if matches {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((p.to_path_buf(), mtime));
            }
        }

        // 按 mtime 倒序排列;**同 mtime 时按路径字典序兜底**。
        // 原实现只有 `sort_by_key(Reverse(mtime))`:粗粒度 mtime 文件系统
        // (HFS+ / FAT32 为 1s 分辨率)上大量文件 mtime 相同,而
        // `sort_by_key` 是稳定排序 → 输出顺序等于 walker 的文件系统枚举
        // 顺序,不确定且不可复现。加二级键后同 mtime 也有确定顺序。
        paths.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let truncated = paths.len() > MAX_RESULTS;
        paths.truncate(MAX_RESULTS);

        // 0.6:输出相对路径 —— 绝对路径每一行都带一遍完整 workspace 前缀。
        let workspace = ctx.workspace_path();
        let mut body = paths
            .iter()
            .map(|(p, _)| crate::sandbox::display_path_relative(p, &workspace))
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            body.push_str(&format!("\n[... truncated to {MAX_RESULTS} ...]"));
        }

        Ok(ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "matches": paths.len(),
                "truncated": truncated,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)] // pre-M5
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_ctx(workspace: &std::path::Path) -> ToolContext {
        ToolContext::for_workspace(workspace)
    }

    fn tmp_workspace() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("zhuye_glob_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn matches_extension() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.rs"), "").unwrap();
        std::fs::write(ws.join("b.txt"), "").unwrap();
        let t = GlobTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("a.rs"));
                assert!(!text.contains("b.txt"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn matches_substring() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/main.rs"), "").unwrap();
        std::fs::write(ws.join("other.rs"), "").unwrap();
        let t = GlobTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "src"}))
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("src/main.rs"));
            }
            _ => panic!("expected text"),
        }
    }

    // ── 0.6 回归:path 参数沙箱化 ─────────────────────────────────

    /// 回归(0.6):`path` 指向 workspace 外必须被拒。
    /// 修复前 glob 的 `path` 直接喂给 `WalkBuilder::new(root)`,
    /// 遍历整个文件系统并把绝对路径吐给模型。
    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = tmp_workspace();
        let outside = ws.parent().unwrap().join("zhuye_glob_outside_probe");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.rs"), "").unwrap();
        let t = GlobTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "*.rs", "path": outside.to_string_lossy()}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathEscape { .. }),
            "workspace 外的 path 必须被拒,得到: {err:?}"
        );
        // 相对穿越同样要拒。
        let err2 = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "*.rs", "path": "../zhuye_glob_outside_probe"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err2, ToolError::PathEscape { .. }),
            "相对穿越必须被拒,得到: {err2:?}"
        );
    }

    /// 回归(0.6):不存在的 path 应给 InvalidArgs 而非裸 Io 错误。
    #[tokio::test]
    async fn rejects_missing_path_with_clear_error() {
        let ws = tmp_workspace();
        let t = GlobTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "*.rs", "path": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { .. }),
            "不存在的 path 应报 InvalidArgs,得到: {err:?}"
        );
    }

    /// 回归:workspace 内的子目录 path 仍须放行。
    #[tokio::test]
    async fn allows_subdir_path_inside_workspace() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/a.rs"), "").unwrap();
        std::fs::write(ws.join("b.rs"), "").unwrap();
        let t = GlobTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "*.rs", "path": "src"}),
            )
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("a.rs"), "应找到子目录内文件: {text}");
                assert!(!text.contains("b.rs"), "不应越过 path 根: {text}");
            }
            _ => panic!("expected text"),
        }
    }

    /// 回归:输出应为相对 workspace 的路径(不带冗长绝对前缀)。
    #[tokio::test]
    async fn output_paths_are_relative() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.rs"), "").unwrap();
        let t = GlobTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert_eq!(text.trim(), "a.rs", "应输出相对路径,得到: {text}");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn ignores_target_dir() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("target/build.rs"), "").unwrap();
        std::fs::write(ws.join("a.rs"), "").unwrap();
        let t = GlobTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(!text.contains("target"));
            }
            _ => panic!("expected text"),
        }
    }
}
