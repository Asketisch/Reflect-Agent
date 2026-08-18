//! `glob` —— 查找匹配 glob 模式的文件。
//!
//! 详见 `docs/tools-and-hooks.md §2.6`。M3 v0 用 `ignore::WalkBuilder`
//! 遍历 workspace;`pattern` 在相对路径上做子串匹配(v0 阶段为节省成本,
//! 完整的 globset 匹配推迟到 v1)。

use std::path::PathBuf;

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
        reflect_prompt::copy("tool.glob")
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
        let root: PathBuf = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.workspace_path());

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
            let matches = if pattern.contains('/') {
                rel_str.contains(pattern)
            } else {
                rel_str.contains(pattern)
                    || (!suffix.is_empty() && p.extension().is_some_and(|e| e == suffix))
            };
            if matches {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((p.to_path_buf(), mtime));
            }
        }

        // 按 mtime 倒序排列。
        paths.sort_by(|a, b| b.1.cmp(&a.1));
        let truncated = paths.len() > MAX_RESULTS;
        paths.truncate(MAX_RESULTS);

        let mut body = paths
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            body.push_str(&format!("\n[... truncated to {MAX_RESULTS} ...]"));
        }

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(body)],
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
            std::env::temp_dir().join(format!("reflect_glob_test_{}_{}", std::process::id(), n));
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
            reflect_protocol::ContentBlock::Text { text } => {
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
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("src/main.rs"));
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
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(!text.contains("target"));
            }
            _ => panic!("expected text"),
        }
    }
}
