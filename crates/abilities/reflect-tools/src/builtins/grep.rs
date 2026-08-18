//! `grep` —— 全 workspace 正则搜索。
//!
//! 详见 `docs/tools-and-hooks.md §2.5`。M3 v0 用 `ignore::WalkBuilder`
//! 遍历 workspace(遵守 `.gitignore`);结果按 `path:line:match` 行格式输出。
//!
//! v1.1.0 P1 `bm25-search`:`rank=bm25` 时对 `pattern` 做 BM25 相关性排序,
//! 替代默认的正则首次命中顺序。

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::Value;

use crate::bm25::{rank_lines, tokenize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX_RESULTS: usize = 100;
const IGNORE_DIRS: &[&str] = &["target", "node_modules", ".git"];
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        reflect_prompt::copy("tool.grep")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern, or BM25 query terms when rank=bm25"},
                "path": {"type": "string", "description": "Search root (default: workspace)"},
                "include": {"type": "string", "description": "Glob to include (e.g. '*.rs')"},
                "max_results": {"type": "integer", "minimum": 0, "description": "Max matches (default 100)"},
                "rank": {
                    "type": "string",
                    "enum": ["regex", "bm25"],
                    "description": "Result ordering: regex (default) or bm25 relevance ranking"
                }
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
        let include_glob = args
            .get("include")
            .and_then(|v| v.as_str())
            .map(String::from);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let root: PathBuf = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.workspace_path());
        let use_bm25 = args
            .get("rank")
            .and_then(|v| v.as_str())
            .is_some_and(|r| r.eq_ignore_ascii_case("bm25"));

        if use_bm25 {
            return execute_bm25(&root, pattern, include_glob.as_deref(), max_results);
        }

        let re = Regex::new(pattern).map_err(|e| ToolError::InvalidArgs {
            message: format!("bad regex: {e}"),
        })?;

        let mut out_lines: Vec<String> = Vec::new();
        let mut total_matches: usize = 0;
        let mut truncated = false;
        let walker = build_walker(&root);
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if !path_matches_include(entry.path(), include_glob.as_deref()) {
                continue;
            }
            let path = entry.path();
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    total_matches += 1;
                    if out_lines.len() < max_results {
                        out_lines.push(format!("{}:{}:{}", path.display(), i + 1, line));
                    } else {
                        truncated = true;
                    }
                }
            }
        }

        Ok(format_grep_output(
            out_lines,
            total_matches,
            max_results,
            truncated,
            "regex",
        ))
    }
}

fn build_walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .standard_filters(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !IGNORE_DIRS.iter().any(|d| name == *d)
        })
        .build()
}

fn path_matches_include(path: &Path, include_glob: Option<&str>) -> bool {
    let Some(glob) = include_glob else {
        return true;
    };
    let path_str = path.to_string_lossy();
    path_str.ends_with(glob.trim_start_matches("*.")) || path_str.ends_with(glob)
}

/// BM25 模式:收集含任一 query term 的行,按相关性降序返回。
fn execute_bm25(
    root: &Path,
    query: &str,
    include_glob: Option<&str>,
    max_results: usize,
) -> Result<ToolOutput, ToolError> {
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return Err(ToolError::InvalidArgs {
            message: "BM25 query has no searchable terms".into(),
        });
    }

    let mut out_lines: Vec<String> = Vec::new();
    let mut total_matches: usize = 0;
    let mut truncated = false;
    let walker = build_walker(root);

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if !path_matches_include(entry.path(), include_glob) {
            continue;
        }
        let path = entry.path();
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };

        let mut candidates: Vec<(usize, String)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line_tokens = tokenize(line);
            if query_terms
                .iter()
                .any(|term| line_tokens.iter().any(|t| t == term))
            {
                candidates.push((i + 1, line.to_string()));
            }
        }
        if candidates.is_empty() {
            continue;
        }

        let ranked = rank_lines(&query_terms, &candidates, BM25_K1, BM25_B);
        for hit in ranked {
            total_matches += 1;
            if out_lines.len() < max_results {
                out_lines.push(format!(
                    "{:.3} {}:{}: {}",
                    hit.score,
                    path.display(),
                    hit.line_no,
                    hit.line
                ));
            } else {
                truncated = true;
            }
        }
    }

    // 跨文件全局再按 BM25 分数降序(行格式以 score 开头)。
    out_lines.sort_by(|a, b| {
        let score_a = a
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok());
        let score_b = b
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok());
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if out_lines.len() > max_results {
        out_lines.truncate(max_results);
        truncated = true;
    }

    Ok(format_grep_output(
        out_lines,
        total_matches,
        max_results,
        truncated,
        "bm25",
    ))
}

fn format_grep_output(
    out_lines: Vec<String>,
    total_matches: usize,
    max_results: usize,
    truncated: bool,
    rank_mode: &str,
) -> ToolOutput {
    let mut body = out_lines.join("\n");
    if truncated {
        body.push_str(&format!(
            "\n[... {} matches omitted ...]",
            total_matches.saturating_sub(max_results)
        ));
    }

    ToolOutput {
        content: vec![reflect_protocol::ContentBlock::text(body)],
        is_error: false,
        metadata: serde_json::json!({
            "total_matches": total_matches,
            "shown": out_lines.len(),
            "truncated": truncated,
            "rank": rank_mode,
        }),
        elapsed_ms: 0,
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
            std::env::temp_dir().join(format!("reflect_grep_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matching_lines() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "alpha\nbeta\ngamma").unwrap();
        std::fs::write(ws.join("b.txt"), "alpha\nother").unwrap();
        let t = GrepTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "alpha"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("a.txt:1:alpha"));
                assert!(text.contains("b.txt:1:alpha"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn bm25_ranks_by_relevance() {
        let ws = tmp_workspace();
        std::fs::write(
            ws.join("a.rs"),
            "fn helper() {}\nasync fn main() {}\nasync fn main() { work().await; }\n",
        )
        .unwrap();
        let t = GrepTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({
                    "pattern": "async fn main",
                    "rank": "bm25",
                    "include": "*.rs"
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["rank"], "bm25");
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("a.rs:"));
                assert!(text.contains("async fn main"));
                // 分数前缀应存在
                assert!(text.chars().next().is_some_and(|c| c.is_ascii_digit()));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn bm25_rejects_empty_query() {
        let ws = tmp_workspace();
        let t = GrepTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "!!!", "rank": "bm25"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn max_results_truncates() {
        let ws = tmp_workspace();
        let body: String = (0..50)
            .map(|i| format!("match{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ws.join("a.txt"), &body).unwrap();
        let t = GrepTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"pattern": "match", "max_results": 5}),
            )
            .await
            .unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("[... 45 matches omitted ...]"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn ignores_target_dir() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("a.txt"), "TODO").unwrap();
        std::fs::write(ws.join("target/b.txt"), "TODO").unwrap();
        let t = GrepTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "TODO"}))
            .await
            .unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("a.txt"));
                assert!(!text.contains("target/b.txt"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn rejects_invalid_regex() {
        let ws = tmp_workspace();
        let t = GrepTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"pattern": "("}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
