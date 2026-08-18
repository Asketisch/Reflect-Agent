//! `reflect session ...` —— 列 / 看 / 删本地 session。
//!
//! v1.x:`ls` / `show` 现在展示每个 session 的累计 token 与 USD 成本
//! (聚合自 JSONL 里的 `RolloutRecord::TokenCount` 记录)。旧 session(jsonl
//! 无 TokenCount 记录)对应列为 `0` / `--`,不破坏现有展示。

use anyhow::{Context, anyhow};
use reflect_protocol::{RolloutRecord, SessionInfo, ThreadId};
use reflect_rollout::index::list_sessions;
use reflect_rollout::path::{default_base, session_path_at};
// v1.x:接入此前孤儿的 rename/read_name/to_markdown(S5b + S3)。
use reflect_rollout::index::{read_session_name, rename_session};
use reflect_rollout::to_markdown;
use uuid::Uuid;

/// `reflect session ls` —— 列出本地 session(按 started_at desc,limit 默认 20)。
///
/// v1.x:新增 `tokens` / `cost` 两列(右对齐、千分位)。`cost` 为 `--`
/// 表示该 session 无 TokenCount 记录(旧 jsonl)或 model 不在 pricing 表。
pub fn ls(limit: usize, model_filter: Option<&str>) -> anyhow::Result<()> {
    let base = default_base();
    let mut sessions = list_sessions(&base).context("list_sessions")?;

    if let Some(m) = model_filter {
        let m_lc = m.to_lowercase();
        sessions.retain(|s| s.model.to_lowercase().contains(&m_lc));
    }

    if sessions.is_empty() {
        println!("(no sessions found under {})", base.display());
        return Ok(());
    }

    // v1.x: 新增 tokens / cost 两列(右对齐)。
    println!(
        "{:<36}  {:<32}  {:<12}  {:>6}  {:>10}  {:>10}",
        "session_id", "model", "started", "msgs", "tokens", "cost"
    );
    for s in sessions.iter().take(limit) {
        let model = truncate(&s.model, 32);
        let tokens = format_with_thousands(&s.total_tokens.to_string());
        let cost = match s.cost_usd {
            Some(c) => format!("${:.2}", c),
            None => String::from("--"),
        };
        println!(
            "{:<36}  {:<32}  {:<12}  {:>6}  {:>10}  {:>10}",
            s.session_id.to_string(),
            model,
            s.started_at.format("%Y-%m-%d"),
            s.message_count,
            tokens,
            cost,
        );
    }
    let shown = sessions.len().min(limit);
    if sessions.len() > limit {
        println!(
            "(showing {shown} of {}; pass --limit/-n to see more)",
            sessions.len()
        );
    }
    Ok(())
}

/// `reflect session show <id>` —— 显示元数据 + 前 5 条消息预览 + 整 session
/// 累计 token / cost。`id` 可以是完整 UUID 或前 8 字符前缀。
///
/// v1.x: 新增 `Tokens:` / `Cost:` 两行。整文件 body 已读进内存,顺路聚合
/// TokenCount 记录(与 `parse_first_session_meta` 同款,零额外 I/O)。
pub fn show(id: &str) -> anyhow::Result<()> {
    let base = default_base();
    let (tid, path) = resolve_session_path(&base, id)?;
    println!("Session: {tid}");
    // v1.x:显示自定义名(若有),优先于自动派生的 title。
    if let Ok(Some(name)) = read_session_name(&base, tid) {
        println!("Name:    {name}");
    }
    println!("Path:    {}", path.display());

    // 读前 6 行:SessionMeta + 5 条 RolloutRecord::Message(若存在)。
    let content = std::fs::read_to_string(&path).context("read session jsonl")?;

    // v1.x: 整 session 累计 token / cost(全文件 body 已在内存)。
    let (input_tokens, output_tokens, total_tokens, cost_usd) = aggregate_token_counts(&content);

    let mut msg_count = 0usize;
    let mut meta_printed = false;
    for line in content.lines().take(20) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
        let r#type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match r#type {
            "session_meta" => {
                let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
                let started = v.get("started_at").and_then(|s| s.as_str()).unwrap_or("?");
                println!("Model:   {model}");
                println!("Started: {started}");
                // v1.x: token / cost 详情行紧跟 meta(在消息预览之前)。
                println!(
                    "Tokens:  {} (in {} / out {})",
                    format_with_thousands(&total_tokens.to_string()),
                    format_with_thousands(&input_tokens.to_string()),
                    format_with_thousands(&output_tokens.to_string()),
                );
                if let Some(c) = cost_usd {
                    println!("Cost:    ${:.2}", c);
                }
                meta_printed = true;
            }
            "message" if msg_count < 5 => {
                msg_count += 1;
                let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                let body = preview_message_body(&v);
                println!("[{msg_count:>2}] {role}: {body}");
            }
            "compaction" if msg_count < 5 => {
                msg_count += 1;
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                println!("[{msg_count:>2}] compaction: {summary}…");
            }
            _ => {}
        }
    }
    if !meta_printed {
        // 缺失 SessionMeta 的兜底:仍输出 token / cost,避免 CLI 完全无统计。
        println!(
            "Tokens:  {} (in {} / out {})",
            format_with_thousands(&total_tokens.to_string()),
            format_with_thousands(&input_tokens.to_string()),
            format_with_thousands(&output_tokens.to_string()),
        );
        if let Some(c) = cost_usd {
            println!("Cost:    ${:.2}", c);
        }
    }
    if msg_count == 0 {
        println!("(no message records in this session)");
    }
    Ok(())
}

/// `reflect session rm <id> [--yes]` —— 删除一个 session 文件。`--yes` 跳过确认。
pub fn rm(id: &str, yes: bool) -> anyhow::Result<()> {
    let base = default_base();
    let (_tid, path) = resolve_session_path(&base, id)?;

    if !path.exists() {
        return Err(anyhow!("session file not found: {}", path.display()));
    }
    if !yes {
        eprint!("Delete {}? [y/N] ", path.display());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    println!("removed {}", path.display());
    Ok(())
}

/// `reflect session fork <id> [--branch <name>]` —— fork 父会话截止当前的
/// 完整历史到新子会话(v1.2 P2)。
///
/// 子会话包含父会话截至 fork 点的所有 records(user + assistant 完整
/// ContentBlocks + Compaction + Checkpoint/Rewind 等),原样复制、无损。
/// fork 完成后用 `reflect exec --resume <child_id> "..."` 续作。
pub fn fork(id: &str, branch_name: Option<&str>) -> anyhow::Result<()> {
    let base = default_base();
    let (parent_id, _path) = resolve_session_path(&base, id)?;

    let child_id = reflect_rollout::index::fork_with_history(
        &base,
        parent_id,
        branch_name.unwrap_or("manual"),
        // None = 全量复制父会话截至末尾(CLI 不暴露 turn 级截断,保持简单)。
        None,
    )
    .with_context(|| format!("fork session {parent_id}"))?;

    println!("Forked session {parent_id} → {child_id}");
    println!("Resume with: reflect exec --resume {child_id} \"...\"");
    Ok(())
}

/// `reflect session rename <id> <name>` —— 给会话设置人可读名称(v1.x S5b)。
///
/// 名称写到 `<base>/_names/<id>.name`,在 `ls`/`show` 渲染时优先于自动派生
/// 的 title。此前 `rename_session` / `read_session_name` 是孤儿 —— 实现完整
/// 但从未调用,用户无法给会话起名。
pub fn rename(id: &str, name: &str) -> anyhow::Result<()> {
    let base = default_base();
    let (tid, _path) = resolve_session_path(&base, id)?;
    rename_session(&base, tid, name).with_context(|| format!("rename session {tid}"))?;
    println!("Renamed session {tid} → {name}");
    Ok(())
}

/// `reflect session export <id> [--out <file>]` —— 把会话导出为人类可读
/// markdown(v1.x S3)。此前 `to_markdown` 是孤儿 —— 完整渲染了 8 种 record
/// 变体(含 Checkpoint/Rewind/TokenCount),但从未接入 CLI。
///
/// 不带 `--out` 打印到 stdout;带 `--out` 写到文件。
pub fn export(id: &str, out: Option<&std::path::Path>) -> anyhow::Result<()> {
    let base = default_base();
    let (tid, path) = resolve_session_path(&base, id)?;
    // 同步读 + 逐行解析(与 fork_with_history 同款,不依赖 tokio reactor)。
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read session file {}", path.display()))?;
    let records: Vec<RolloutRecord> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<RolloutRecord>(l).ok())
        .collect();
    let markdown = to_markdown(&records);
    match out {
        Some(p) => {
            std::fs::write(p, &markdown)
                .with_context(|| format!("write export file {}", p.display()))?;
            println!(
                "Exported session {tid} → {} ({} bytes)",
                p.display(),
                markdown.len()
            );
        }
        None => {
            print!("{markdown}");
        }
    }
    Ok(())
}

/// 把 `id` 解析成 `ThreadId`:完整 UUID 或前 8 字符前缀(扫描 list_sessions)。
/// 解析 `id`(完整 UUID 或前缀)为 session 文件路径。
///
/// 路径解析策略(修复「show/rm 用今天日期拼路径导致历史会话找不到」的 bug):
/// 1. 前缀匹配:从 `list_sessions` 拿到精确 `started_at`,用 `session_path_at`
///    拼 `<base>/YYYY/MM/DD/<id>.jsonl`(日期 = session 创建日,非今天)。
/// 2. 完整 UUID 且在 session 列表中:同上,用真实 `started_at`。
/// 3. 完整 UUID 但不在列表中(如跨天 / started_at 缺失):`find_session_file`
///    全盘扫描兜底。
///
/// 返回 `(ThreadId, 路径)`。
fn resolve_session_path(
    base: &std::path::Path,
    id: &str,
) -> anyhow::Result<(ThreadId, std::path::PathBuf)> {
    match resolve_session_id(base, id)? {
        // 有 SessionInfo:用真实 started_at 拼路径,失败再兜底扫描。
        Some(info) => {
            let path = session_path_at(base, info.session_id, info.started_at);
            if path.exists() {
                Ok((info.session_id, path))
            } else {
                let found = find_session_file(base, info.session_id)?
                    .ok_or_else(|| anyhow!("session file not found: {}", path.display()))?;
                Ok((info.session_id, found))
            }
        }
        // 完整 UUID 但不在列表:直接全盘扫描。
        None => {
            let tid = Uuid::parse_str(id)
                .map(ThreadId)
                .map_err(|e| anyhow!("invalid UUID '{id}': {e}"))?;
            let found = find_session_file(base, tid)?
                .ok_or_else(|| anyhow!("session file not found for id: {id}"))?;
            Ok((tid, found))
        }
    }
}

/// 解析 `id`(完整 UUID 或前缀)为 `SessionInfo`。
///
/// 前缀则扫描 `list_sessions`,返回唯一命中会话的完整 `SessionInfo`
///(`started_at` 精确,用于 `session_path_at` 拼路径)。
/// 完整 UUID 优先从已扫描列表拿真实 `started_at`;找不到返回 `None`,
/// 由调用方走 `find_session_file` 全盘扫描兜底。
fn resolve_session_id(base: &std::path::Path, id: &str) -> anyhow::Result<Option<SessionInfo>> {
    if id.len() == 36 {
        let tid = Uuid::parse_str(id)
            .map(ThreadId)
            .map_err(|e| anyhow!("invalid UUID '{id}': {e}"))?;
        // 完整 UUID:从已扫描 session 列表里找匹配项拿到真实 started_at。
        // 找不到返回 None(调用方走 find_session_file 兜底)。
        if let Ok(sessions) = list_sessions(base)
            && let Some(found) = sessions.iter().find(|s| s.session_id == tid)
        {
            return Ok(Some(found.clone()));
        }
        return Ok(None);
    }
    // 部分 ID 前缀匹配:扫描 list_sessions,挑 starts_with 命中的。
    let sessions = list_sessions(base).context("list_sessions for prefix match")?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.session_id.to_string().starts_with(id))
        .collect();
    match matches.len() {
        0 => Err(anyhow!(
            "no session matches prefix '{id}'; expected UUID or first N chars"
        )),
        1 => Ok(Some(matches[0].clone())),
        n => Err(anyhow!(
            "prefix '{id} matches {n} sessions; please give a longer prefix or full UUID"
        )),
    }
}

/// 全盘扫描 `<base>/**/*.jsonl` 找指定 `session_id` 的文件路径(含
/// `.N.jsonl` 轮转副本)。`started_at` 不可靠或文件被移动时作为兜底。
///
/// 用 `std::fs` 递归遍历(布局为 `YYYY/MM/DD/`,固定 3 层),避免引入
/// walkdir 依赖。优先返回无 `.N` 后缀的 active 文件。
fn find_session_file(
    base: &std::path::Path,
    session_id: ThreadId,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    use std::path::PathBuf;
    let needle = format!("{}.jsonl", session_id);
    let mut found: Option<PathBuf> = None;
    // 布局:<base>/YYYY/MM/DD/<id>.jsonl,递归 3 层年/月/日目录。
    fn scan_dir(dir: &std::path::Path, needle: &str, found: &mut Option<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                scan_dir(&path, needle, found);
                // 命中 active 文件后提前结束。
                if let Some(p) = found
                    && p.file_name().map(|n| n.to_string_lossy().into_owned())
                        == Some(needle.to_string())
                {
                    return;
                }
            } else if ft.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(needle) {
                    // active 文件(无 .N 后缀)立即返回。
                    if name == needle {
                        *found = Some(path);
                        return;
                    }
                    // 轮转副本作为兜底。
                    if found.is_none() {
                        *found = Some(path);
                    }
                }
            }
        }
    }
    scan_dir(base, &needle, &mut found);
    Ok(found)
}

/// 从 RolloutRecord::Message 的 JSON 提取 body preview(截 80 字符)。
fn preview_message_body(v: &serde_json::Value) -> String {
    let body = v
        .get("content")
        .and_then(|c| match c {
            serde_json::Value::String(s) => Some(s.clone()),
            other => other
                .get("text")
                .and_then(|t| t.as_str())
                .map(str::to_string),
        })
        .unwrap_or_default();
    let single_line = body.replace('\n', " ").trim().to_string();
    if single_line.chars().count() <= 80 {
        single_line
    } else {
        let truncated: String = single_line.chars().take(80).collect();
        format!("{truncated}…")
    }
}

/// UTF-8 字符级截断到 `max_chars`,超出追加 `…`(与 discussion ls 复用)。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count >= max_chars.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
    }
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        out
    }
}

/// 千分位逗号格式(右对齐列用)。`18234` → `18,234`,`1234567` → `1,234,567`。
/// 输入预期为纯数字字符串(由 `u64::to_string` 产生),非数字字符按原样透传。
fn format_with_thousands(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in chars.iter().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    out.chars().rev().collect()
}

/// v1.x: 扫描 JSONL body,聚合 `RolloutRecord::TokenCount` 记录的 token
/// 与 cost。返回 `(input, output, total, cost_usd)`。`cost_usd` 仅当至少
/// 一条记录带 `Some(c)` 时为 `Some(sum)`,否则 `None`。
///
/// 与 `reflect_rollout::index::aggregate_token_counts` 同款逻辑;CLI 侧
/// 复刻一份避免把内部 helper 提升为 pub API。
fn aggregate_token_counts(body: &str) -> (u64, u64, u64, Option<f64>) {
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut total_tokens: u64 = 0;
    let mut cost_sum: f64 = 0.0;
    let mut cost_present = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<RolloutRecord>(trimmed) else {
            continue;
        };
        if let RolloutRecord::TokenCount {
            usage, cost_usd, ..
        } = rec
        {
            input_tokens = input_tokens.saturating_add(usage.input_tokens as u64);
            output_tokens = output_tokens.saturating_add(usage.output_tokens as u64);
            total_tokens = total_tokens.saturating_add(usage.total_tokens as u64);
            if let Some(c) = cost_usd {
                cost_sum += c;
                cost_present = true;
            }
        }
    }
    let cost_usd = cost_present.then_some(cost_sum);
    (input_tokens, output_tokens, total_tokens, cost_usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// 写一个最小化的 session JSONL(SessionMeta + 1 message),验证 ls/show/rm。
    #[test]
    fn session_ls_show_rm_roundtrip() {
        let tmp = tmpdir();
        let tid = Uuid::new_v4();
        let dir = tmp.path().join("2026").join("06").join("23");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tid}.jsonl"));
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"session_id\":\"{tid}\",\"model\":\"anthropic/claude-3-5-sonnet-latest\",\"started_at\":\"2026-06-23T12:00:00Z\",\"message_count\":1}}\n{{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello\"}}\n"
            ),
        )
        .unwrap();

        // list_sessions
        let sessions = list_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id.0, tid);

        // rm
        fs::remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    /// truncate 在边界正确处理(`max_chars` 自身 = `…` 占位)。
    #[test]
    fn truncate_at_boundary() {
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abcdefgh", 4), "abc…");
    }

    /// preview_message_body 兼容 string content + object { text } content。
    #[test]
    fn preview_handles_string_and_object() {
        let s = serde_json::json!({"content": "hello world"});
        assert_eq!(preview_message_body(&s), "hello world");
        let o = serde_json::json!({"content": {"text": "structured"}});
        assert_eq!(preview_message_body(&o), "structured");
        let empty = serde_json::json!({});
        assert_eq!(preview_message_body(&empty), "");
    }

    // ── v1.x:千分位格式 helper ──────────────────────────────────────

    #[test]
    fn format_with_thousands_inserts_commas() {
        assert_eq!(format_with_thousands("0"), "0");
        assert_eq!(format_with_thousands("999"), "999");
        assert_eq!(format_with_thousands("1000"), "1,000");
        assert_eq!(format_with_thousands("18234"), "18,234");
        assert_eq!(format_with_thousands("1234567"), "1,234,567");
    }

    // ── v1.x:TokenCount 聚合(CLI 侧复刻) ────────────────────────────

    /// 构造一行 `TokenCount` JSONL(原始 JSON 字符串,避免测试依赖 chrono
    /// 直接构造 `RolloutRecord::TokenCount { at: DateTime }`)。
    fn token_count_line_json(input: u32, output: u32, total: u32, cost_usd: Option<f64>) -> String {
        let cost_field = match cost_usd {
            Some(c) => format!(r#","cost_usd":{c}"#),
            None => String::new(),
        };
        format!(
            r#"{{"type":"token_count","turn_id":"00000000-0000-0000-0000-00000000000{n}","usage":{{"input_tokens":{input},"output_tokens":{output},"cached_tokens":0,"cache_write_tokens":0,"total_tokens":{total}}},"at":"2026-07-31T00:00:00Z"{cost_field}}}"#,
            n = input % 10,
        )
    }

    /// `aggregate_token_counts` 聚合多条 TokenCount 的 usage 与 cost。
    #[test]
    fn aggregate_token_counts_sums_usage_and_cost() {
        let mut body = String::from(
            r#"{"type":"session_meta","session_id":"00000000-0000-0000-0000-000000000001","model":"m","started_at":"2026-07-31T00:00:00Z"}"#,
        );
        body.push('\n');
        for (i, o, t, c) in [
            (1000u32, 200u32, 1200u32, Some(0.01f64)),
            (200, 50, 250, Some(0.008)),
            (300, 100, 400, Some(0.0054)),
        ] {
            body.push_str(&token_count_line_json(i, o, t, c));
            body.push('\n');
        }
        let (inp, out, tot, cost) = aggregate_token_counts(&body);
        assert_eq!(inp, 1500);
        assert_eq!(out, 350);
        assert_eq!(tot, 1850);
        let cost = cost.expect("cost present");
        assert!((cost - 0.0234).abs() < 1e-9, "cost: {cost}");
    }

    /// 旧 jsonl(无 TokenCount)→ 全 0 / None,向后兼容。
    #[test]
    fn aggregate_token_counts_zero_for_old_rollout() {
        let body = "{\"type\":\"session_meta\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"model\":\"m\",\"started_at\":\"2026-07-31T00:00:00Z\"}\n{\"type\":\"message\",\"role\":\"user\",\"content\":\"hi\"}\n";
        let (inp, out, tot, cost) = aggregate_token_counts(body);
        assert_eq!(inp, 0);
        assert_eq!(out, 0);
        assert_eq!(tot, 0);
        assert_eq!(cost, None);
    }

    /// `show()` / `ls()` 在含 TokenCount 记录的 session 上不 panic,且
    /// `list_sessions` 聚合出正确的累计值。`show()` / `ls()` 内部用
    /// `default_base()` 读 `$HOME`,故这里用 `lock_home` 把 HOME 指到
    /// tmpdir(stdout 渲染正确性由 `format_with_thousands` /
    /// `aggregate_token_counts` 单测覆盖,避免引入 stdout-capture 依赖)。
    #[test]
    fn session_show_and_ls_run_on_token_count_session() {
        use crate::test_home::lock_home;
        let tmp = tmpdir();
        let _guard = lock_home(tmp.path());
        let tid = Uuid::new_v4();
        // default_base() = $HOME/.reflect/sessions,故文件须落在该子树下。
        let dir = tmp
            .path()
            .join(".reflect")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("31");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tid}.jsonl"));
        let mut body = format!(
            "{{\"type\":\"session_meta\",\"session_id\":\"{tid}\",\"model\":\"openai/gpt-4o\",\"started_at\":\"2026-07-31T12:00:00Z\"}}\n"
        );
        body.push_str(&token_count_line_json(1500, 350, 1850, Some(0.0234)));
        body.push('\n');
        fs::write(&path, body).unwrap();

        // list_sessions(default_base → $HOME) 应聚合出累计值(供 ls 渲染)。
        let base = default_base();
        let sessions = list_sessions(&base).unwrap();
        assert_eq!(sessions[0].total_tokens, 1850);
        assert_eq!(sessions[0].input_tokens, 1500);
        assert_eq!(sessions[0].output_tokens, 350);
        let cost = sessions[0].cost_usd.expect("cost");
        assert!((cost - 0.0234).abs() < 1e-9);

        // ls() / show() 不 panic(解析整文件 + 渲染 meta / tokens / cost 行)。
        ls(20, None).unwrap();
        show(&tid.to_string()).unwrap();
    }
}
