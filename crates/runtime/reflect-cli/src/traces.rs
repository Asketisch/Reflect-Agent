//! `reflect traces ...` —— 列 / 看本地 LLM 调用记录(`~/.reflect/traces/`)。
//!
//! 与 `session` 子命令对称:数据来自 `reflect-telemetry` 的 model-io JSONL
//! (每次 LLM 调用一条 `ModelIoRecord`)。`ls` 列 session 文件 + 调用次数 +
//! 最近时间;`show` 展开某 session 的逐条调用详情(model / latency / usage /
//! query_source + request/response 摘要)。
//!
//! session_id → 可读名:fallback 链 = rollout 自定义名 → rollout title →
//! session_id 前 8 字符。

use reflect_rollout::index::list_sessions;
use reflect_telemetry::{list_model_io_files, read_model_io_file, resolve_traces_dir};

/// `reflect traces ls` —— 列出本地 LLM 调用 session(按最近修改 desc,
/// limit 默认 20)。
pub fn ls(limit: usize) -> anyhow::Result<()> {
    let base = resolve_traces_dir(None);
    let files = list_model_io_files(&base);
    if files.is_empty() {
        println!("(no traces found under {})", base.display());
        return Ok(());
    }

    // 读 rollout session 列表,用于把 session_id 映射成可读 title。
    let sessions = list_sessions(&reflect_rollout::path::default_base()).unwrap_or_default();

    println!(
        "{:<36}  {:<20}  {:<5}  {:<12}  {:<8}",
        "session_id", "title", "calls", "last", "model"
    );
    for (id, path, _mtime) in files.iter().take(limit) {
        // 读该 session 全部记录,取调用数 + 最近一条的 model / 时间。
        let recs = read_model_io_file(path);
        let calls = recs.len();
        let last = recs
            .last()
            .map(|r| r.completed_at.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into());
        let model = recs
            .last()
            .map(|r| r.model.model_id.clone())
            .unwrap_or_default();
        // 可读名 fallback:rollout title → session_id 前 8 字符。
        let title = sessions
            .iter()
            .find(|s| s.session_id.to_string() == *id)
            .and_then(|s| s.title.clone())
            .unwrap_or_else(|| id.chars().take(8).collect());
        println!(
            "{:<36}  {:<20}  {:<5}  {:<12}  {}",
            id,
            truncate(&title, 20),
            calls,
            last,
            truncate(&model, 24),
        );
    }
    let shown = files.len().min(limit);
    if files.len() > limit {
        println!(
            "(showing {shown} of {}; pass --limit/-n to see more)",
            files.len()
        );
    }
    Ok(())
}

/// `reflect traces show <id>` —— 显示某 session 的逐条 LLM 调用详情。
/// `id` 可以是完整 UUID 或前 8 字符前缀。
pub fn show(id: &str) -> anyhow::Result<()> {
    let base = resolve_traces_dir(None);
    let path = resolve_model_io_path(&base, id)?;
    let recs = read_model_io_file(&path);
    if recs.is_empty() {
        println!("(no model-io records in this session)");
        return Ok(());
    }

    println!("Session: {id}");
    println!("Path:    {}", path.display());
    println!("Calls:   {}", recs.len());
    println!();

    for (i, r) in recs.iter().enumerate() {
        println!(
            "── [{i:>3}] {} | {} | {}ms | attempt {} ──",
            r.completed_at.format("%Y-%m-%d %H:%M:%S"),
            r.query_source,
            r.duration_ms,
            r.attempt,
        );
        println!(
            "  model:    {} ({})",
            r.model.model_id,
            r.model.provider_id.as_deref().unwrap_or("?"),
        );
        println!(
            "  usage:    in={} out={} cached={} total={} cost={}",
            r.usage.input_tokens,
            r.usage.output_tokens,
            r.usage.cached_tokens,
            r.usage.total_tokens,
            r.usage
                .cost_usd
                .map(|c| format!("${c:.4}"))
                .unwrap_or_else(|| "-".into()),
        );
        // request/response 摘要:截断长文本。
        let req_preview = preview_value(&r.request);
        let resp_preview = preview_value(&r.response);
        println!("  request:  {}", req_preview);
        println!("  response: {}", resp_preview);
        println!();
    }
    Ok(())
}

/// 把 `id`(完整 UUID 或前缀)解析为 model-io 文件路径。
/// 前缀匹配扫描 `list_model_io_files`,要求唯一命中。
fn resolve_model_io_path(base: &std::path::Path, id: &str) -> anyhow::Result<std::path::PathBuf> {
    let files = list_model_io_files(base);
    // 完整匹配优先。
    if let Some((_, path, _)) = files.iter().find(|(sid, _, _)| sid == id) {
        return Ok(path.clone());
    }
    // 前缀匹配。
    let matches: Vec<_> = files
        .iter()
        .filter(|(sid, _, _)| sid.starts_with(id))
        .collect();
    match matches.len() {
        0 => anyhow::bail!("no trace session matches '{id}'"),
        1 => Ok(matches[0].1.clone()),
        n => anyhow::bail!("prefix '{id}' matches {n} sessions; give a longer prefix or full UUID"),
    }
}

/// 把 serde_json::Value 压成单行预览(截 120 字符),便于终端展示。
fn preview_value(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".into());
    let single = s.replace('\n', " ");
    if single.chars().count() <= 120 {
        single
    } else {
        let truncated: String = single.chars().take(120).collect();
        format!("{truncated}…")
    }
}

/// UTF-8 字符级截断(超长追加 `…`)。与 session.rs 同语义。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn truncate_short_and_long() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn preview_value_truncates_long() {
        let long = serde_json::json!({"text": "x".repeat(300)});
        let p = preview_value(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= 121); // 120 + …
    }

    #[test]
    fn preview_value_preserves_short() {
        let v = serde_json::json!({"finish_reason": "stop"});
        assert_eq!(preview_value(&v), r#"{"finish_reason":"stop"}"#);
    }

    /// ls/show 在空目录下不 panic(返回 "(no traces found)")。
    #[test]
    fn ls_empty_dir_ok() {
        let tmp = tempdir().unwrap();
        // resolve_traces_dir(None) 用 $HOME,这里直接测 list_model_io_files。
        let files = list_model_io_files(tmp.path());
        assert!(files.is_empty());
    }

    /// roundtrip:写一个 model-io 文件,验证 list + read 能正确解析。
    #[test]
    fn list_and_read_roundtrip() {
        let tmp = tempdir().unwrap();
        let base = tmp.path();
        let mio_dir = base.join("model-io");
        fs::create_dir_all(&mio_dir).unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let path = mio_dir.join(format!("model-io-sess_{sid}.jsonl"));
        // 一条最小化 ModelIoRecord JSON。
        let rec = format!(
            r#"{{"started_at":"2026-07-31T00:00:00Z","completed_at":"2026-07-31T00:00:01Z","duration_ms":1000,"attempt":1,"request_id":"r1","trace_id":"t1","session_id":"{sid}","query_source":"main_turn","model":{{"model_id":"glm-5.2","provider_id":"builtin","role":"main","source":"main_turn"}},"request":{{}},"response":{{"finish_reason":"stop"}},"usage":{{"input_tokens":10,"output_tokens":5,"cached_tokens":0,"cache_write_tokens":0,"total_tokens":15}}}}"#
        );
        fs::write(&path, rec).unwrap();

        let files = list_model_io_files(base);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, sid);

        let recs = read_model_io_file(&path);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].usage.total_tokens, 15);
        assert_eq!(recs[0].model.model_id, "glm-5.2");

        // 前缀解析。
        let resolved = resolve_model_io_path(base, "11111111").unwrap();
        assert_eq!(resolved, path);
    }
}
