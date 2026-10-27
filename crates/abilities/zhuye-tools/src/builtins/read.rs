//! `read` —— 读取文本文件(带行号、二进制检测与截断)。
//!
//! 详见 `docs/tools-and-hooks.md §2.2`。

use std::fs;
use std::io::Read;

use async_trait::async_trait;
use serde_json::Value;

use crate::sandbox::resolve_sandbox_path;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// 二进制检测嗅探字节上限。
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// 行数软上限(超出后做摘要截断)。
const TRUNCATION_THRESHOLD: usize = 2000;
const TRUNCATION_HEAD: usize = 1000;
const TRUNCATION_TAIL: usize = 1000;

/// 0.9:`offset` / `limit` 的 schema 上界与运行时钳制值。
///
/// 取 `usize::MAX / 2` 而非 `usize::MAX`:这样 `offset + limit` 即使在
/// 钳制被绕过的情况下也**不可能**回绕(两个都 ≤ MAX/2,和 ≤ MAX),
/// 从数值上消除 slice 越界的可能。
const MAX_LINE_INDEX: usize = usize::MAX / 2;

/// 0.9:读入内存前的文件大小上限。
///
/// 原实现 `fs::read_to_string` 无上限,而 `TRUNCATION_THRESHOLD` 只限制
/// "显示多少行"、不限制"读入多少字节" —— 一个 10GB 文本文件会直接把
/// 进程 OOM。超过上限时给出可操作错误(引导用 grep + offset 分段)。
const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.read")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to workspace, or absolute path under workspace."},
                // 0.9:schema 补 `maximum`。原实现只有 `minimum: 0`,模型传
                // `limit: 18446744073709551615` 时 `offset + limit` 在
                // release 下回绕 → `start > end` → slice 索引 panic。
                "offset": {"type": "integer", "minimum": 0, "maximum": MAX_LINE_INDEX, "description": "Start line (0-indexed)"},
                "limit": {"type": "integer", "minimum": 0, "maximum": MAX_LINE_INDEX, "description": "Max lines to read"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        // 0.9:上限钳制 + `checked_add`。原实现是 `offset + l` 裸加,
        // release 下回绕成 0,而 `start = offset.min(total)` 仍是 offset
        // → `lines[offset..0]` 直接 panic("slice index starts at .. but
        // ends at ..")。debug 下则在加法处就 overflow panic。
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_LINE_INDEX as u64) as usize;
        let limit: Option<usize> = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n.min(MAX_LINE_INDEX as u64)) as usize);

        let path = std::path::Path::new(path_str);
        let abs = resolve_sandbox_path(&ctx.workspace_path(), path)?;

        // 二进制嗅探。
        let mut sniff = vec![0u8; BINARY_SNIFF_BYTES];
        let mut f = fs::File::open(&abs)?;
        let n = f.read(&mut sniff)?;
        if sniff[..n].contains(&0u8) {
            return Ok(ToolOutput {
                content: vec![zhuye_protocol::ContentBlock::text(
                    "binary file detected; use grep for binary content",
                )],
                is_error: true,
                metadata: serde_json::json!({"path": path_str, "binary": true}),
                elapsed_ms: 0,
            });
        }
        drop(f);

        // 0.9:读入前先查文件大小,避免把超大文件整体读进内存。
        // 原来的 `fs::read_to_string` 无上限,一个 10GB 文本文件直接 OOM;
        // 而 TRUNCATION_THRESHOLD(2000 行)限制的是"显示",不是"读入"。
        if let Ok(meta) = fs::metadata(&abs)
            && meta.len() > MAX_READ_BYTES
        {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "文件过大({} 字节 > {} 字节上限):请用 grep 定位,再用 offset/limit 分段读",
                    meta.len(),
                    MAX_READ_BYTES
                ),
            });
        }

        let raw =
            fs::read_to_string(&abs).map_err(|e| ToolError::Io(format!("read {path_str}: {e}")))?;
        let lines: Vec<&str> = raw.lines().collect();
        let total = lines.len();
        // 0.9:`checked_add` 双保险 —— 即使上面的钳制被绕过也不会回绕。
        let end = match limit {
            Some(l) => offset.checked_add(l).unwrap_or(total).min(total),
            None => total,
        };
        let slice: Vec<&str> = lines[offset.min(total)..end].to_vec();
        let body = if slice.len() > TRUNCATION_THRESHOLD {
            let head = &slice[..TRUNCATION_HEAD];
            let tail = &slice[slice.len() - TRUNCATION_TAIL..];
            let omitted = slice.len() - TRUNCATION_HEAD - TRUNCATION_TAIL;
            let mut s = String::new();
            for (i, l) in head.iter().enumerate() {
                s.push_str(&format!("{:>6}\t{}\n", offset + i + 1, l));
            }
            s.push_str(&format!("\n[... {omitted} lines omitted ...]\n\n"));
            for (i, l) in tail.iter().enumerate() {
                s.push_str(&format!(
                    "{:>6}\t{}\n",
                    offset + head.len() + omitted + i + 1,
                    l
                ));
            }
            s
        } else {
            let mut s = String::new();
            for (i, l) in slice.iter().enumerate() {
                s.push_str(&format!("{:>6}\t{}\n", offset + i + 1, l));
            }
            s
        };
        Ok(ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "path": abs.to_string_lossy(),
                "path_str": path_str,
                "mtime_ms": std::fs::metadata(&abs)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64),
                "total_lines": total,
                "shown_lines": slice.len(),
                "truncated": slice.len() > TRUNCATION_THRESHOLD,
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
            std::env::temp_dir().join(format!("zhuye_read_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn reads_text_file_with_line_numbers() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "alpha\nbeta\ngamma").unwrap();
        let t = ReadTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("alpha"));
                assert!(text.contains("beta"));
                assert!(text.contains("gamma"));
                // 行号从 1 开始(1-indexed)
                assert!(text.contains("     1\t"));
                assert!(text.contains("     2\t"));
                assert!(text.contains("     3\t"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn detects_binary_file() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let t = ReadTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"path": "b.bin"}))
            .await
            .unwrap();
        assert!(out.is_error);
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("binary"))
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = tmp_workspace();
        let t = ReadTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"path": "/etc/passwd"}))
            .await
            .unwrap_err();
        // /etc/passwd 在 macOS 上存在,但位于临时目录之外(应被沙箱拒绝)。
        assert!(matches!(
            err,
            ToolError::PathEscape { .. } | ToolError::Io(_)
        ));
    }

    #[tokio::test]
    async fn rejects_missing_path_arg() {
        let ws = tmp_workspace();
        let t = ReadTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let ws = tmp_workspace();
        let body: String = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ws.join("big.txt"), &body).unwrap();
        let t = ReadTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "big.txt", "offset": 10, "limit": 5}),
            )
            .await
            .unwrap();
        match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("line10"));
                assert!(text.contains("line14"));
                assert!(!text.contains("line15"));
            }
            _ => panic!("expected text"),
        }
    }

    /// Phase 4:成功 read 在 metadata 暴露 `path`(canonical 绝对路径)
    /// 与 `mtime_ms`,供 `ReadBeforeEditHook` 的 PostToolUse 标记。
    #[tokio::test]
    async fn metadata_exposes_canonical_path_and_mtime() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        let t = ReadTool;
        let out = t
            .execute(make_ctx(&ws), serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        let path_val = out
            .metadata
            .get("path")
            .and_then(|v| v.as_str())
            .expect("metadata.path present");
        // canonical 应是绝对路径,不是 "a.txt"。
        assert!(
            std::path::Path::new(path_val).is_absolute(),
            "metadata.path 应是 canonical 绝对路径,got {path_val:?}"
        );
        assert!(
            path_val.ends_with("a.txt"),
            "metadata.path 应以 a.txt 结尾,got {path_val:?}"
        );
        let mtime_ms = out
            .metadata
            .get("mtime_ms")
            .and_then(|v| v.as_u64())
            .expect("metadata.mtime_ms present");
        // mtime_ms 应该是合理的 epoch 时间戳(> 1.7e12 即 2024+)
        assert!(mtime_ms > 1_700_000_000_000, "got {mtime_ms}");
    }
    // ── 0.9 回归:offset/limit 溢出 ─────────────────────────────────

    /// 回归(0.9):`limit` 极大时不得 panic。
    /// 修复前 `offset + l` 在 release 下回绕成 0,而
    /// `start = offset.min(total)` 仍是 offset → `lines[offset..0]`
    /// 触发 "slice index starts at .. but ends at .." panic。
    #[tokio::test]
    async fn huge_limit_does_not_panic() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "l1\nl2\nl3\n").unwrap();
        let t = ReadTool;
        for limit in [u64::MAX, usize::MAX as u64, 1 << 63] {
            let out = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "a.txt", "offset": 1, "limit": limit}),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "limit={limit} 应正常返回");
        }
    }

    /// 回归(0.9):`offset` 极大时同样不得 panic(超出总行数 → 空结果)。
    #[tokio::test]
    async fn huge_offset_does_not_panic() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "l1\nl2\n").unwrap();
        let t = ReadTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "offset": u64::MAX, "limit": 10}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    /// 回归(0.9):offset + limit 同时极大(回绕的最坏组合)。
    #[tokio::test]
    async fn huge_offset_and_limit_together_does_not_panic() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "l1\n").unwrap();
        let t = ReadTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "offset": u64::MAX, "limit": u64::MAX}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    /// 回归:schema 必须声明 `maximum`,否则弱模型不知道有上界。
    #[test]
    fn schema_declares_upper_bounds() {
        let t = ReadTool;
        let schema = t.parameters_schema();
        for k in ["offset", "limit"] {
            let max = schema["properties"][k]["maximum"].as_u64();
            assert!(max.is_some(), "{k} 必须声明 maximum");
            assert_eq!(max, Some((MAX_LINE_INDEX) as u64));
        }
    }
}
