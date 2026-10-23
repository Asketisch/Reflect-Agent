//! 跨会话内容搜索(v1.5 新功能)。
//!
//! 流式扫描 `<base>` 目录树下的全部 JSONL,对包含 `query`(大小写
//! 不敏感子串)的消息记录返回一条命中摘要。定位场景:用户想找回
//! "上次那个报错 / 修法出现在哪个 session" —— 是一切 resume 工作流
//! 的检索入口(`session ls` 只能看首行预览,几十个会话后靠肉眼找
//! 历史几乎不可用)。
//!
//! 设计要点:
//! - **纯读**:不改任何文件,零协议改动;
//! - **按 mtime 优先**:最近活跃的会话文件先扫,命中数达上限即提前收工;
//! - **防泄漏**:命中摘要在返回前过 [`reflect_sanitize::redact_string`]
//!   (与 rollout 落盘脱敏同一套 10 类密钥 pattern),不会把密钥带进
//!   搜索输出。

use std::io::Read as _;
use std::path::{Path, PathBuf};

use reflect_protocol::{MessageRole, ThreadId};

/// 单条搜索命中。
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// 命中记录所属 session(取自文件首行 `SessionMeta`;缺失时为
    /// 文件名 stem 的 UUID 解析兜底,再缺失为 `None`)。
    pub session_id: Option<ThreadId>,
    /// 命中文件路径(完整,可直接喂给 `session show` / `--resume`)。
    pub file: PathBuf,
    /// 命中行号(1-based,JSONL 行)。
    pub line_no: usize,
    /// 消息角色(`Message` 命中时:`user` / `assistant` / `tool`;其余变体为 `None`)。
    pub role: Option<String>,
    /// 命中摘要(含查询词的上下文片段,已过密钥脱敏)。
    pub snippet: String,
}

/// 搜索选项。
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// 命中数上限(达到即停止扫描,默认 50)。
    pub max_hits: usize,
    /// 最多扫描的文件数(按 mtime 优先;防大会话目录失控,默认 500)。
    pub max_files: usize,
    /// 摘要两侧上下文字符数(默认 64)。
    pub context_chars: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_hits: 50,
            max_files: 500,
            context_chars: 64,
        }
    }
}

/// 在 `base` 目录树下搜索包含 `query`(大小写不敏感子串)的记录。
///
/// 返回按「文件 mtime 倒序 × 行号正序」排列的命中列表(最近活跃
/// 的会话在前);命中数达 `opts.max_hits` 即提前返回。
pub fn search_sessions(
    base: &Path,
    query: &str,
    opts: &SearchOptions,
) -> std::io::Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_jsonl(base, &mut files)?;
    // mtime 倒序:最近活跃优先。mtime 不可用的文件排最后仍参与扫描。
    files.sort_by_key(|p| {
        let mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok();
        std::cmp::Reverse(mtime)
    });
    files.truncate(opts.max_files);

    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    for path in &files {
        let session_id = super::index::first_line_session_id(path);
        for hit in scan_file(path, &needle, session_id, opts)? {
            hits.push(hit);
            if hits.len() >= opts.max_hits {
                return Ok(hits);
            }
        }
    }
    Ok(hits)
}

/// 扫描单个 JSONL 文件中的所有命中行。
fn scan_file(
    path: &Path,
    needle: &str,
    session_id: Option<ThreadId>,
    opts: &SearchOptions,
) -> std::io::Result<Vec<SearchHit>> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Ok(Vec::new());
    };
    let mut body = String::new();
    if f.read_to_string(&mut body).is_err() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let Some(hit) = scan_line(line, needle, path, i + 1, session_id, opts) else {
            continue;
        };
        hits.push(hit);
    }
    Ok(hits)
}

/// 扫描单行:可解析的记录提取文本字段做匹配;不可解析的行跳过
/// (避免命中 JSON 内部键名产生假阳性)。
fn scan_line(
    line: &str,
    needle: &str,
    path: &Path,
    line_no: usize,
    session_id: Option<ThreadId>,
    opts: &SearchOptions,
) -> Option<SearchHit> {
    let record: reflect_protocol::RolloutRecord = serde_json::from_str(line).ok()?;
    let (role, text) = searchable_text(&record)?;
    let lower = text.to_lowercase();
    let pos = lower.find(needle)?;
    // 摘要:命中位置前后各留 context_chars 字符;切点按字符边界(避免
    // 多字节字符切半产生乱码)。
    let from = {
        let raw = pos.saturating_sub(opts.context_chars);
        let mut b = raw;
        while b > 0 && !text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };
    let to = {
        let raw = (pos + needle.len())
            .saturating_add(opts.context_chars)
            .min(text.len());
        let mut e = raw;
        while e < text.len() && !text.is_char_boundary(e) {
            e += 1;
        }
        e
    };
    let mut snippet = String::new();
    if from > 0 {
        snippet.push('…');
    }
    // 摘要压缩换行:JSONL 记录的文本可能含多行,命中文本保持单行可读。
    for ch in text[from..to].chars() {
        if ch == '\n' || ch == '\r' {
            snippet.push(' ');
        } else {
            snippet.push(ch);
        }
    }
    if to < text.len() {
        snippet.push('…');
    }
    // 密钥脱敏:与 rollout 落盘同一套 pattern,搜索输出不成为新的泄漏面。
    let snippet = reflect_sanitize::redact_string(&snippet);
    Some(SearchHit {
        session_id,
        file: path.to_path_buf(),
        line_no,
        role,
        snippet,
    })
}

/// 提取记录的可搜索文本。`(role 标签, 文本)`;无可搜索文本返回 `None`。
fn searchable_text(record: &reflect_protocol::RolloutRecord) -> Option<(Option<String>, String)> {
    match record {
        reflect_protocol::RolloutRecord::Message { role, content, .. } => {
            let role = match role {
                MessageRole::System => Some("system".to_string()),
                MessageRole::User => Some("user".to_string()),
                MessageRole::Assistant => Some("assistant".to_string()),
                MessageRole::Tool => Some("tool".to_string()),
            };
            // content 是 Value:字符串或 ContentBlock 数组,递归收集全部字符串叶。
            let mut parts = Vec::new();
            collect_strings(content, &mut parts);
            if parts.is_empty() {
                return None;
            }
            Some((role, parts.join(" ")))
        }
        // 压缩摘要也可能被检索(找"那段讨论去哪了")。
        reflect_protocol::RolloutRecord::Compaction { summary, .. } => {
            Some((Some("compaction".to_string()), summary.clone()))
        }
        reflect_protocol::RolloutRecord::Fork { branch_name, .. } => {
            Some((Some("fork".to_string()), branch_name.clone()))
        }
        _ => None,
    }
}

/// 递归收集 JSON value 中的全部字符串叶。
fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for i in items {
                collect_strings(i, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, val) in map.iter() {
                collect_strings(val, out);
            }
        }
        _ => {}
    }
}

/// 递归收集 `base` 下全部 `.jsonl` 文件。
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{RolloutRecord, TurnId};
    use tempfile::tempdir;

    /// 写一个最小 session 文件(SessionMeta + 多条 message)。
    fn write_session(dir: &Path, name: &str, texts: &[&str]) -> PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        let sid = ThreadId::new();
        writeln!(
            f,
            "{}",
            serde_json::to_string(&RolloutRecord::session_meta(sid, "mock/model")).unwrap()
        )
        .unwrap();
        for t in texts {
            let rec = RolloutRecord::message(
                TurnId::new(),
                reflect_protocol::MessageRole::User,
                serde_json::json!(t),
            );
            writeln!(f, "{}", serde_json::to_string(&rec).unwrap()).unwrap();
        }
        path
    }

    #[test]
    fn finds_substring_case_insensitive() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "a.jsonl",
            &["hello Reflect World", "other line"],
        );
        let hits = search_sessions(dir.path(), "reflect", &SearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role.as_deref(), Some("user"));
        assert!(hits[0].snippet.contains("Reflect"));
        assert_eq!(hits[0].line_no, 2);
        assert!(hits[0].session_id.is_some());
    }

    #[test]
    fn newest_file_scanned_first() {
        let dir = tempdir().unwrap();
        let old = write_session(dir.path(), "old.jsonl", &["needle one"]);
        let new = write_session(dir.path(), "new.jsonl", &["needle two"]);
        // 显式设置 mtime:old 更旧。
        let t_old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let t_new = std::time::SystemTime::now();
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(t_old)).unwrap();
        filetime::set_file_mtime(&new, filetime::FileTime::from_system_time(t_new)).unwrap();
        let hits = search_sessions(dir.path(), "needle", &SearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].file.ends_with("new.jsonl"),
            "最新文件应先扫: {:?}",
            hits[0].file
        );
    }

    #[test]
    fn max_hits_stops_early() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", &["hit"; 10]);
        let hits = search_sessions(
            dir.path(),
            "hit",
            &SearchOptions {
                max_hits: 3,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn multibyte_query_and_snippet_no_panic() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "c.jsonl", &["关键结论：零成本抽象才是正解"]);
        let hits = search_sessions(dir.path(), "零成本", &SearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("零成本"));
    }

    #[test]
    fn secret_in_message_is_redacted_in_snippet() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "d.jsonl",
            &["key is sk-ant-api03-abcdefghij very secret context"],
        );
        let hits = search_sessions(dir.path(), "secret", &SearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            !hits[0].snippet.contains("sk-ant-api03-abcdefghij"),
            "搜索摘要不得泄漏密钥: {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn empty_base_returns_empty() {
        let dir = tempdir().unwrap();
        let hits = search_sessions(dir.path(), "x", &SearchOptions::default()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_query_returns_empty() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", &["x"]);
        let hits = search_sessions(dir.path(), "  ", &SearchOptions::default()).unwrap();
        assert!(hits.is_empty());
    }
}
