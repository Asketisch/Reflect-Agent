//! `ReadNotes` —— 读 coordinator scratchpad 下的 note 文件。
//!
//! 参数:`name?`(不传则列出所有 note 的标题 + 前 200 字摘要;
//! 传则返回该 note 的全文,超过 64 KiB 截断)。
//!
//! 行为:扫描 `<scratchpad_root>/*.md`,按文件名排序。`name` 指定时
//! 只读那个文件;不存在返 `is_error = false` + "note not found"。
//!
//! 权限:`Auto`,与 `WriteNote` 同 —— scratchpad 是 session-private 临时目录。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;
use tracing::warn;

/// `ReadNotes` 工具:list 模式下每条 note 摘要最大字符数。
const SUMMARY_CHARS: usize = 200;

/// 单条 note 全文读回时的字节上限。超过会被截断。
const MAX_FULL_READ_BYTES: usize = 64 * 1024;

/// `ReadNotes` 工具实现。
pub struct ReadNotesTool {
    scratchpad_root: Arc<PathBuf>,
}

impl std::fmt::Debug for ReadNotesTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadNotesTool")
            .field(
                "scratchpad_root",
                &self.scratchpad_root.display().to_string(),
            )
            .finish()
    }
}

impl ReadNotesTool {
    pub fn new(scratchpad_root: Arc<PathBuf>) -> Self {
        Self { scratchpad_root }
    }
}

#[async_trait]
impl Tool for ReadNotesTool {
    fn name(&self) -> &str {
        "ReadNotes"
    }

    fn description(&self) -> &str {
        "读 coordinator scratchpad 下的 note 文件。name 不传则列出所有 \
         note 的标题与摘要(前 200 字);传 name 则返回该 note 的全文 \
         (超过 64 KiB 会截断)。note 不存在时返 is_error=false + 'note not found'。"
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
                    "description": "Note 名(不含 .md 后缀);省略则列出全部"
                }
            }
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 多 reader 并发安全(只读 fs)。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let name_filter = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // 全在 spawn_blocking 里做 fs 操作,避免阻塞 tokio runtime。
        let root = self.scratchpad_root.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<ReadOutcome, std::io::Error> {
            let root = root.as_path();
            if !root.exists() {
                return Ok(ReadOutcome::NotFound {
                    reason:
                        "scratchpad directory does not exist (coordinator mode may not be enabled)"
                            .into(),
                });
            }
            match name_filter {
                Some(name) => {
                    let p = root.join(format!("{name}.md"));
                    match std::fs::read_to_string(&p) {
                        Ok(mut body) => {
                            let truncated = body.len() > MAX_FULL_READ_BYTES;
                            if truncated {
                                let mut idx = MAX_FULL_READ_BYTES;
                                while !body.is_char_boundary(idx) && idx > 0 {
                                    idx -= 1;
                                }
                                body.truncate(idx);
                            }
                            Ok(ReadOutcome::Single {
                                name,
                                path: p.display().to_string(),
                                body,
                                truncated,
                            })
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Ok(ReadOutcome::NotFound {
                                reason: format!("note '{name}' not found"),
                            })
                        }
                        Err(e) => Err(e),
                    }
                }
                None => {
                    let mut entries: Vec<(String, String, String)> = Vec::new();
                    for entry in std::fs::read_dir(root)? {
                        let entry = entry?;
                        let path = entry.path();
                        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        if !fname.ends_with(".md") {
                            continue;
                        }
                        let stem = fname.trim_end_matches(".md").to_string();
                        let body = std::fs::read_to_string(&path).unwrap_or_default();
                        let summary: String = body.chars().take(SUMMARY_CHARS).collect();
                        entries.push((stem, path.display().to_string(), summary));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    Ok(ReadOutcome::List(entries))
                }
            }
        })
        .await
        .map_err(|e| ToolError::Execution(format!("spawn_blocking join: {e}")))?;

        let result = match result {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "ReadNotes failed");
                return Err(ToolError::Io(e.to_string()));
            }
        };

        match result {
            ReadOutcome::NotFound { reason } => Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(reason.clone())],
                is_error: false,
                metadata: serde_json::json!({"found": false, "reason": reason}),
                elapsed_ms: 0,
            }),
            ReadOutcome::Single {
                name,
                path,
                body,
                truncated,
            } => Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(body.clone())],
                is_error: false,
                metadata: serde_json::json!({
                    "found": true,
                    "name": name,
                    "path": path,
                    "length": body.len(),
                    "truncated": truncated,
                }),
                elapsed_ms: 0,
            }),
            ReadOutcome::List(entries) => {
                let count = entries.len();
                let summary = if entries.is_empty() {
                    "No notes found in scratchpad".to_string()
                } else {
                    let mut s = String::new();
                    for (name, _path, sum) in &entries {
                        s.push_str(&format!("## {name}\n"));
                        s.push_str(sum);
                        if sum.len() < sum.chars().take(SUMMARY_CHARS).collect::<String>().len() {
                            s.push_str("…\n");
                        } else {
                            s.push('\n');
                        }
                        s.push('\n');
                    }
                    s
                };
                Ok(ToolOutput {
                    content: vec![reflect_protocol::ContentBlock::text(summary)],
                    is_error: false,
                    metadata: serde_json::json!({
                        "found": true,
                        "mode": "list",
                        "count": count,
                        "notes": entries,
                    }),
                    elapsed_ms: 0,
                })
            }
        }
    }
}

/// `ReadNotes` 内部读结果枚举,避免 spawn_blocking 闭包返回复杂 trait object。
enum ReadOutcome {
    Single {
        name: String,
        path: String,
        body: String,
        truncated: bool,
    },
    List(Vec<(String, String, String)>),
    NotFound {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn read_single_returns_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("note.md"), "hello world").unwrap();
        let tool = ReadNotesTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "note"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["found"], true);
        assert_eq!(out.metadata["name"], "note");
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn read_missing_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let tool = ReadNotesTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "ghost"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["found"], false);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("not found"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn read_list_returns_summary() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("alpha.md"), "first body").unwrap();
        std::fs::write(dir.path().join("beta.md"), "second body").unwrap();
        let tool = ReadNotesTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["count"], 2);
        let names: Vec<&str> = out.metadata["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e[0].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn read_list_returns_empty_when_no_notes() {
        let dir = TempDir::new().unwrap();
        let tool = ReadNotesTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["count"], 0);
    }

    #[tokio::test]
    async fn read_truncates_oversized() {
        let dir = TempDir::new().unwrap();
        let big = "y".repeat(MAX_FULL_READ_BYTES + 100);
        std::fs::write(dir.path().join("big.md"), &big).unwrap();
        let tool = ReadNotesTool::new(Arc::new(dir.path().to_path_buf()));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "big"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["truncated"], true);
        assert!(out.metadata["length"].as_u64().unwrap() <= MAX_FULL_READ_BYTES as u64);
    }

    #[tokio::test]
    async fn read_handles_missing_scratchpad_dir() {
        let dir = TempDir::new().unwrap();
        let non_existent = dir.path().join("does_not_exist");
        let tool = ReadNotesTool::new(Arc::new(non_existent));
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["found"], false);
    }

    #[test]
    fn metadata_is_stable() {
        let tool = ReadNotesTool::new(Arc::new(PathBuf::from("/tmp/x")));
        assert_eq!(tool.name(), "ReadNotes");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
        let schema = tool.parameters_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        // name 是 optional
        assert!(
            schema.get("required").is_none() || schema["required"].as_array().unwrap().is_empty()
        );
    }
}
