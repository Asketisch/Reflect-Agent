//! `ActiveFileRecovery` — post-compact 自动重读最近修改过的文件。
//!
//! 扫描 `AIMessage.tool_calls`,过滤 write / replace / edit 类工具,反向
//! 遍历 + dedup,cap 10 文件,50k token 总预算 + 5k token / 文件。
//!
//! ## 设计差异
//!
//! - 本实现把
//!   提取(返回 `Vec<(String, String)>`)和渲染(返回 `String`)
//!   拆成两个方法,方便单测 + 让 `recovery_meta_to_messages` 统一去重。
//! - 预算单位对齐 `reflect-compact::tokens::estimate_messages` 的
//!   `chars / 3.5` 公式 —— `tokens * 7 / 2 = chars`。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reflect_llm::ChatMessage;
use serde_json::Value;

/// 默认识别为"修改了文件"的工具名集合。
///
/// 硬编码列表:
/// `write` / `replace` / `write_file` / `edit_notebook`。
/// 本实现把 `edit_notebook` 替换为更通用的 `edit_file` + `replace_in_file`。
pub const DEFAULT_WRITE_TOOLS: &[&str] = &[
    "write",
    "write_file",
    "edit",
    "edit_file",
    "replace",
    "replace_in_file",
    "multiedit",
];

/// 最多恢复多少个文件(超过截断)。
pub const ACTIVE_FILES_MAX_FILES: usize = 10;

/// 总 token 预算(50k tokens ≈ 175_000 chars)。
pub const ACTIVE_FILES_TOKEN_BUDGET: u32 = 50_000;

/// 单文件 token 上限(5k tokens ≈ 17_500 chars)。
pub const ACTIVE_FILES_PER_FILE_TOKEN_CAP: u32 = 5_000;

/// 单文件原始字节上限(20KB)。超过直接 skip,避免读大文件。
const ACTIVE_FILES_MAX_BYTES: u64 = 20_000;

/// `recover_with_deleted` 的「成功读出」分支 —— `(path, content)` 对。
pub type RecoveredFile = (String, String);

/// `recover_with_deleted` 的「失败但路径有效」分支 —— `(path, reason)` 对,
/// 其中 `reason` 是简短人类可读原因。
pub type DeletedFile = (String, &'static str);

/// 简单文件路径提取(无任何 fs 访问)。
pub struct ActiveFileRecovery {
    pub workspace: Arc<Path>,
    pub tool_names: Arc<HashSet<String>>,
    pub max_files: usize,
    pub max_tokens_total: u32,
    pub max_tokens_per_file: u32,
}

impl std::fmt::Debug for ActiveFileRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveFileRecovery")
            .field("workspace", &self.workspace)
            .field("tool_names", &self.tool_names)
            .field("max_files", &self.max_files)
            .field("max_tokens_total", &self.max_tokens_total)
            .field("max_tokens_per_file", &self.max_tokens_per_file)
            .finish()
    }
}

impl Default for ActiveFileRecovery {
    fn default() -> Self {
        // 默认 workspace 走当前目录(测试用);生产 `bootstrap_m4` 注入
        // 真正的 `Arc<Path>`。
        Self {
            workspace: Arc::from(PathBuf::from(".")),
            tool_names: Arc::new(DEFAULT_WRITE_TOOLS.iter().map(|s| s.to_string()).collect()),
            max_files: ACTIVE_FILES_MAX_FILES,
            max_tokens_total: ACTIVE_FILES_TOKEN_BUDGET,
            max_tokens_per_file: ACTIVE_FILES_PER_FILE_TOKEN_CAP,
        }
    }
}

impl ActiveFileRecovery {
    /// 用默认工具集 (`DEFAULT_WRITE_TOOLS`) 构造。
    pub fn new(workspace: Arc<Path>) -> Self {
        Self {
            workspace,
            ..Self::default()
        }
    }

    /// 覆盖工具名集合(测试 / 特殊项目用)。
    pub fn with_tool_names(mut self, names: &[&str]) -> Self {
        self.tool_names = Arc::new(names.iter().map(|s| s.to_string()).collect());
        self
    }

    /// 覆盖单文件 / 总文件 token 上限(测试用)。
    pub fn with_limits(
        mut self,
        max_files: usize,
        max_tokens_total: u32,
        max_tokens_per_file: u32,
    ) -> Self {
        self.max_files = max_files;
        self.max_tokens_total = max_tokens_total;
        self.max_tokens_per_file = max_tokens_per_file;
        self
    }

    /// 扫描 `messages`,提取最近 write / edit 的文件路径,读出内容,
    /// 按预算截断。返回 `(path, content)` 列表(新→旧顺序)。
    ///
    /// 任何文件读取失败都静默 skip(只 `tracing::debug!`),保证
    /// `pre_loop` 不会因为单文件失败而阻塞。
    pub fn recover(&self, msgs: &[ChatMessage]) -> Vec<(String, String)> {
        // 1. 收集路径:反向扫 Assistant.tool_calls → 过滤 tool_names →
        // 提取 args 里的路径字段 → HashSet 去重 → 截断 max_files。
        let mut ordered: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for msg in msgs.iter().rev() {
            if let ChatMessage::Assistant(a) = msg {
                for tc in &a.tool_calls {
                    if !self.tool_names.contains(&tc.name) {
                        continue;
                    }
                    if let Some(p) = extract_path_from_args(&tc.arguments)
                        && seen.insert(p.clone())
                    {
                        ordered.push(p);
                    }
                }
            }
            if ordered.len() >= self.max_files {
                break;
            }
        }
        // 截断到 max_files(保留最新 N 个)
        ordered.truncate(self.max_files);

        // 2. 读 + 截断每个文件,累计总预算。
        let per_file_chars = (self.max_tokens_per_file * 7 / 2) as usize;
        let total_chars_budget = (self.max_tokens_total * 7 / 2) as usize;
        let mut total_consumed: usize = 0;
        let mut out: Vec<(String, String)> = Vec::new();
        for path in ordered {
            let p = Path::new(&path);
            // 必须绝对路径 + 存在 + ≤ 20KB
            if !p.is_absolute() {
                tracing::debug!(path = %path, "active file skip: 非绝对路径");
                continue;
            }
            let metadata = match std::fs::metadata(p) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!(path = %path, error = %e, "active file skip: metadata 失败");
                    continue;
                }
            };
            if !metadata.is_file() {
                tracing::debug!(path = %path, "active file skip: 非文件");
                continue;
            }
            if metadata.len() > ACTIVE_FILES_MAX_BYTES {
                tracing::debug!(
                    path = %path,
                    bytes = metadata.len(),
                    "active file skip: 超过 20KB 上限"
                );
                continue;
            }
            let raw = match std::fs::read_to_string(p) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(path = %path, error = %e, "active file skip: 读取失败(可能非 UTF-8)");
                    continue;
                }
            };
            // 单文件截断(按字符边界,bytes 数 × 7/2 反推 chars 上限)。
            let truncated = truncate_to_chars(&raw, per_file_chars);
            // 总预算扣减
            let remaining = total_chars_budget.saturating_sub(total_consumed);
            if remaining == 0 {
                tracing::debug!(path = %path, "active file skip: 总预算已用尽");
                break;
            }
            let chars_in_truncated = truncated.chars().count();
            if chars_in_truncated > remaining {
                // 二次截断到剩余预算,然后 break —— 总预算已用尽。
                // 加 `...[truncated]` marker 防止 LLM 误以为文件已完整
                // 装下(否则 LLM 看到内容结束但全文未提"省略",会误用
                // 文件的"前 N 字"当作完整内容做决策)。
                let slice: String = truncated.chars().take(remaining).collect();
                out.push((
                    path,
                    format!("{slice}\n...[budget exhausted at {remaining} chars]..."),
                ));
                break;
            }
            total_consumed += chars_in_truncated;
            out.push((path, truncated));
        }
        out
    }

    /// 把恢复结果渲染为单条 `<system-reminder>` 内容。
    ///
    /// 输出格式:
    /// ```text
    /// Active Files Context (auto-recovered after compaction)
    ///
    /// After the last compaction, the following files were recently written/edited.
    /// Use this to maintain continuity without re-reading from disk:
    ///
    /// === /path/to/file ===
    /// <content>
    /// ```
    pub fn to_meta_message(&self, recovered: &[(String, String)]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        s.push_str("Active Files Context (auto-recovered after compaction)\n\n");
        s.push_str(
            "After the last compaction, the following files were recently written/edited.\n",
        );
        s.push_str("Use this to maintain continuity without re-reading from disk:\n\n");
        for (path, content) in recovered {
            let _ = writeln!(s, "=== {path} ===");
            s.push_str(content);
            if !content.ends_with('\n') {
                s.push('\n');
            }
            s.push('\n');
        }
        s
    }

    /// `recover` 的加强版 —— 同时返回「成功读出」与「被 skip 但路径有效」
    /// 两组文件。后者常见原因:相对路径、文件已删除 / 不存在、非 UTF-8
    /// 二进制、超过 20KB 上限。
    ///
    /// Review 2026-06-29 BUG-5 在 `reflect-core/graph/nodes/mod.rs` 引入此
    /// 入口用于在 `<system-reminder>` 顶部告诉 LLM 哪些文件读不到,避免
    /// LLM 重复 `read` 同样的文件。`recover` 自身语义保持向后兼容。
    ///
    /// 返回元组:`(recovered, deleted)` —— `deleted` 是 `Vec<(path, reason)>`
    /// 其中 `reason` 是简短人类可读原因(`"not found"` / `"not utf-8"` /
    /// `"non-absolute"` / `"too large"` / `"not a file"` / `"budget exhausted"`)。
    pub fn recover_with_deleted(
        &self,
        msgs: &[ChatMessage],
    ) -> (Vec<RecoveredFile>, Vec<DeletedFile>) {
        // 1. 收集路径(与 `recover` 同逻辑)。
        let mut ordered: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for msg in msgs.iter().rev() {
            if let ChatMessage::Assistant(a) = msg {
                for tc in &a.tool_calls {
                    if !self.tool_names.contains(&tc.name) {
                        continue;
                    }
                    if let Some(p) = extract_path_from_args(&tc.arguments)
                        && seen.insert(p.clone())
                    {
                        ordered.push(p);
                    }
                }
            }
            if ordered.len() >= self.max_files {
                break;
            }
        }
        ordered.truncate(self.max_files);

        // 2. 读 + 截断每个文件 —— 不再静默 skip,失败原因进入 `deleted`。
        let per_file_chars = (self.max_tokens_per_file * 7 / 2) as usize;
        let total_chars_budget = (self.max_tokens_total * 7 / 2) as usize;
        let mut total_consumed: usize = 0;
        let mut recovered: Vec<(String, String)> = Vec::new();
        let mut deleted: Vec<(String, &'static str)> = Vec::new();
        for path in ordered {
            let p = Path::new(&path);
            if !p.is_absolute() {
                deleted.push((path, "non-absolute"));
                continue;
            }
            let metadata = match std::fs::metadata(p) {
                Ok(m) => m,
                Err(_) => {
                    deleted.push((path, "not found"));
                    continue;
                }
            };
            if !metadata.is_file() {
                deleted.push((path, "not a file"));
                continue;
            }
            if metadata.len() > ACTIVE_FILES_MAX_BYTES {
                deleted.push((path, "too large"));
                continue;
            }
            let raw = match std::fs::read_to_string(p) {
                Ok(s) => s,
                Err(_) => {
                    deleted.push((path, "not utf-8"));
                    continue;
                }
            };
            let truncated = truncate_to_chars(&raw, per_file_chars);
            let remaining = total_chars_budget.saturating_sub(total_consumed);
            if remaining == 0 {
                deleted.push((path, "budget exhausted"));
                continue;
            }
            let chars_in_truncated = truncated.chars().count();
            let final_content = if chars_in_truncated > remaining {
                // 二次截断到剩余预算 —— `recover` 在此处直接 `break` 不记录,
                // 我们这里改 `continue` 但不计入 `deleted`(内容已被部分保留)。
                let slice: String = truncated.chars().take(remaining).collect();
                total_consumed = total_chars_budget;
                slice
            } else {
                total_consumed += chars_in_truncated;
                truncated
            };
            recovered.push((path, final_content));
        }
        (recovered, deleted)
    }

    /// 渲染 `recover_with_deleted` 的元信息 —— 在 `to_meta_message` 顶部
    /// 加一行 `[N files skipped: ...]`,让 LLM 知道哪些路径读了但拿不到内容。
    ///
    /// `deleted` 列表被裁到最多 10 项(超过显示 `... and N more`),避免
    /// meta 本身塞爆 context。
    pub fn to_meta_message_with_deleted(
        &self,
        recovered: &[RecoveredFile],
        deleted: &[DeletedFile],
    ) -> String {
        let mut prefix = String::new();
        if !deleted.is_empty() {
            prefix.push_str(&format!("[{} files skipped: ", deleted.len()));
            let show_n = deleted.len().min(10);
            for (i, (path, reason)) in deleted.iter().take(show_n).enumerate() {
                if i > 0 {
                    prefix.push_str(", ");
                }
                prefix.push_str(&format!("{path} ({reason})"));
            }
            if deleted.len() > show_n {
                prefix.push_str(&format!(", ... and {} more", deleted.len() - show_n));
            }
            prefix.push_str("]\n");
        }
        let mut body = self.to_meta_message(recovered);
        if !prefix.is_empty() {
            body = format!("{prefix}{body}");
        }
        body
    }
}

/// 从 `serde_json::Value` 里提取文件路径(优先 `path` / `file_path`,
/// fallback `file` / `target` / 任何看起来是绝对路径的字符串)。
fn extract_path_from_args(args: &Value) -> Option<String> {
    for k in ["path", "file_path", "file", "target"] {
        if let Some(v) = args.get(k).and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    // Fallback:扫所有 string 字段,挑第一个绝对路径
    if let Some(map) = args.as_object() {
        for v in map.values() {
            if let Some(s) = v.as_str()
                && Path::new(s).is_absolute()
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 按字符边界截断,加 `...[truncated]` 标记。`max_chars` 是字符数上限
/// (不是字节),避免在多字节 UTF-8 序列中间切。
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut = max_chars.saturating_sub(20);
    let truncated: String = s.chars().take(cut).collect();
    format!("{truncated}\n...[file truncated at {max_chars} chars]...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::{AssistantContent, ToolCallRequest, UserContent};
    use tempfile::TempDir;

    fn make_assistant_msg(name: &str, args: serde_json::Value) -> ChatMessage {
        ChatMessage::Assistant(AssistantContent {
            text: None,
            tool_calls: vec![ToolCallRequest {
                id: "call_1".into(),
                name: name.into(),
                arguments: args,
            }],
            thinking: None,
        })
    }

    #[test]
    fn extract_path_from_args_prefers_path_key() {
        let v = serde_json::json!({"path": "/a/b/c", "file_path": "/wrong"});
        assert_eq!(extract_path_from_args(&v), Some("/a/b/c".into()));
    }

    #[test]
    fn extract_path_falls_back_to_absolute_string() {
        let v = serde_json::json!({"foo": "/abs/path", "bar": "relative"});
        assert_eq!(extract_path_from_args(&v), Some("/abs/path".into()));
    }

    #[test]
    fn extract_path_returns_none_when_no_path() {
        let v = serde_json::json!({"foo": "bar", "count": 3});
        assert_eq!(extract_path_from_args(&v), None);
    }

    #[test]
    fn recover_skips_non_write_tools() {
        let rec = ActiveFileRecovery::default();
        let msgs = vec![
            make_assistant_msg("bash", serde_json::json!({"path": "/etc/passwd"})),
            make_assistant_msg("read", serde_json::json!({"path": "/etc/hosts"})),
        ];
        assert!(rec.recover(&msgs).is_empty());
    }

    #[test]
    fn recover_dedupes_paths_keeping_newest() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        let a_str = a.to_str().unwrap().to_string();
        let b_str = b.to_str().unwrap().to_string();
        let rec = ActiveFileRecovery::default();
        let msgs = vec![
            make_assistant_msg("write", serde_json::json!({"path": &a_str})),
            make_assistant_msg("write", serde_json::json!({"path": &b_str})),
            make_assistant_msg("write", serde_json::json!({"path": &a_str})),
        ];
        // 反向扫:先 a(去重),再 b,再 a(去重)。最终顺序 [a, b]。
        let paths: Vec<String> = rec.recover(&msgs).into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec![a_str, b_str]);
    }

    #[test]
    fn recover_caps_at_max_files() {
        let dir = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for i in 0..10 {
            let p = dir.path().join(format!("file_{i}.txt"));
            std::fs::write(&p, format!("{i}")).unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }
        let msgs: Vec<ChatMessage> = paths
            .iter()
            .map(|p| make_assistant_msg("write", serde_json::json!({"path": p})))
            .collect();
        let rec = ActiveFileRecovery::default().with_limits(3, 50_000, 5_000);
        let result_paths: Vec<String> = rec.recover(&msgs).into_iter().map(|(p, _)| p).collect();
        assert_eq!(result_paths.len(), 3);
    }

    #[test]
    fn recover_skips_relative_and_nonexistent() {
        let rec = ActiveFileRecovery::default();
        let msgs = vec![
            make_assistant_msg("write", serde_json::json!({"path": "relative/x"})),
            make_assistant_msg("write", serde_json::json!({"path": "/nonexistent_99999"})),
        ];
        assert!(rec.recover(&msgs).is_empty());
    }

    #[test]
    fn recover_reads_file_and_returns_content() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        std::fs::write(&p, "hello world").unwrap();
        let rec = ActiveFileRecovery::default();
        let abs = p.to_str().unwrap();
        let msgs = vec![make_assistant_msg(
            "write",
            serde_json::json!({"path": abs}),
        )];
        let result = rec.recover(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, abs);
        assert!(result[0].1.contains("hello world"));
    }

    #[test]
    fn recover_skips_large_files() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("big.txt");
        // 21KB > 20KB 限制
        let big = "x".repeat(21_000);
        std::fs::write(&p, &big).unwrap();
        let rec = ActiveFileRecovery::default();
        let abs = p.to_str().unwrap();
        let msgs = vec![make_assistant_msg(
            "write",
            serde_json::json!({"path": abs}),
        )];
        assert!(rec.recover(&msgs).is_empty());
    }

    #[test]
    fn recover_truncates_per_file_to_token_cap() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("big.txt");
        // 20KB 字符(正好在 20KB byte 限制下,但超过 5k tokens)
        let content = "y".repeat(20_000);
        std::fs::write(&p, &content).unwrap();
        let rec = ActiveFileRecovery::default().with_limits(10, 50_000, 1_000);
        let abs = p.to_str().unwrap();
        let msgs = vec![make_assistant_msg(
            "write",
            serde_json::json!({"path": abs}),
        )];
        let result = rec.recover(&msgs);
        assert_eq!(result.len(), 1);
        // 5_000 token * 7/2 = 17_500,1_000 token * 7/2 = 3_500
        let (path, content) = &result[0];
        assert_eq!(path, abs);
        assert!(
            content.chars().count() <= 3_500 + 50,
            "got {} chars",
            content.chars().count()
        );
        assert!(content.contains("[file truncated"));
    }

    #[test]
    fn recover_respects_total_budget_across_files() {
        let dir = TempDir::new().unwrap();
        let mut msgs = Vec::new();
        for i in 0..5 {
            let p = dir.path().join(format!("f{i}.txt"));
            std::fs::write(&p, "z".repeat(5_000)).unwrap();
            let abs = p.to_str().unwrap().to_string();
            msgs.push(make_assistant_msg(
                "write",
                serde_json::json!({"path": abs}),
            ));
        }
        // 总预算 3_000 tokens(10_500 chars),单文件 1_000 tokens(3_500 chars)
        let rec = ActiveFileRecovery::default().with_limits(10, 3_000, 1_000);
        let result = rec.recover(&msgs);
        // 3 个文件 × 3_500 chars = 10_500,正好打满预算;4 个会超
        // 实际因为总预算 10_500 / 3_500 = 3,只 3 个能完整装下
        assert_eq!(result.len(), 3, "budget should cap at 3 files");
    }

    /// v1.1.0 review P2-1:recover 旧版在「总预算耗尽 + 截断到剩余」分支
    /// 静默 push 切片 + break,LLM 看到内容结束但没"省略"标记,会误用
    /// 文件前 N 字当作完整内容做决策(例如改 build 命令时漏看末尾 flags)。
    /// 修复后加 `...[budget exhausted at N chars]...` 标记,与单文件
    /// token 截断的 `[file truncated at N chars]` 风格一致。
    #[test]
    fn recover_marks_budget_exhausted_truncation() {
        let dir = TempDir::new().unwrap();
        let mut msgs = Vec::new();
        for i in 0..3 {
            let p = dir.path().join(format!("f{i}.txt"));
            // 3_000 chars 内容,单文件 1_000 token (3_500 chars) 不截断,
            // 但总预算 1_000 token (3_500 chars) 装不下 2 个完整文件。
            std::fs::write(&p, "z".repeat(3_000)).unwrap();
            let abs = p.to_str().unwrap().to_string();
            msgs.push(make_assistant_msg(
                "write",
                serde_json::json!({"path": abs}),
            ));
        }
        let rec = ActiveFileRecovery::default().with_limits(10, 1_000, 1_000);
        let result = rec.recover(&msgs);
        // 第 1 个文件(最新)装下,第 2 个预算耗尽被截断,第 3 个不装。
        assert_eq!(result.len(), 2, "第 1 个完整 + 第 2 个截断");
        // 第 2 个文件应有 budget exhausted marker
        let (_, content) = &result[1];
        assert!(
            content.contains("[budget exhausted at"),
            "第 2 个文件应有 budget exhausted 标记,got: {content:?}"
        );
    }

    #[test]
    fn to_meta_message_includes_all_paths() {
        let rec = ActiveFileRecovery::default();
        let recovered = vec![
            ("/a.py".to_string(), "aaa".to_string()),
            ("/b.py".to_string(), "bbb".to_string()),
        ];
        let m = rec.to_meta_message(&recovered);
        assert!(m.contains("Active Files Context"));
        assert!(m.contains("=== /a.py ==="));
        assert!(m.contains("=== /b.py ==="));
        assert!(m.contains("aaa"));
        assert!(m.contains("bbb"));
    }

    #[test]
    fn with_tool_names_overrides_default() {
        let rec = ActiveFileRecovery::default().with_tool_names(&["custom_tool"]);
        assert!(rec.tool_names.contains("custom_tool"));
        assert!(!rec.tool_names.contains("write"));
    }

    #[test]
    fn empty_messages_returns_empty() {
        let rec = ActiveFileRecovery::default();
        let msgs: Vec<ChatMessage> = vec![
            ChatMessage::User(UserContent::default()),
            ChatMessage::System("sys".into()),
        ];
        assert!(rec.recover(&msgs).is_empty());
    }
}
