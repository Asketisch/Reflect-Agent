//! `reflect session ...` —— 列 / 看 / 删本地 session。
//!
//! v1.x:`ls` / `show` 展示每个 session 的累计 token 与 USD 成本
//! (聚合自 JSONL 里的 `RolloutRecord::TokenCount` 记录)。旧 session(jsonl
//! 无 TokenCount 记录)对应列为 `0` / `--`,不破坏现有展示。
//!
//! v1.x 模型级统计:`show` 展示 per-model 用量表;`stats` 跨会话按模型
//! 汇总(每个模型的使用量一览)。统计统一走
//! `reflect_rollout::index::session_stats` / `all_model_usage`,CLI 不再
//! 复刻聚合逻辑。

use anyhow::{Context, anyhow};
use reflect_protocol::{ModelUsage, RolloutRecord, SessionInfo, SessionStats, ThreadId};
use reflect_rollout::index::list_sessions;
use reflect_rollout::path::{default_base, session_path_at};
// v1.x:接入此前孤儿的 rename/read_name/to_markdown(S5b + S3)。
use reflect_rollout::index::{read_session_name, rename_session};
// 双 ID(文件名 thread id vs 首行内部 session_id)兜底解析助手。
use reflect_rollout::index::{
    find_file_by_session_id, find_files_by_name_prefix, first_line_session_id,
};
// v1.x:统一统计聚合(替代此前的 CLI 侧复刻)。
use reflect_rollout::{all_model_usage, index::session_stats, to_markdown};
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
/// 累计 token / cost + per-model 用量表。`id` 可以是完整 UUID 或前 8 字符前缀。
///
/// v1.x:统计统一走 `index::session_stats`(全量扫描活跃 + 轮转文件),
/// 不再 CLI 侧复刻聚合;新增 per-model 表格(每个模型的使用量)。
pub fn show(id: &str) -> anyhow::Result<()> {
    let base = default_base();
    let (tid, path) = resolve_session_path(&base, id)?;
    println!("Session: {tid}");
    // v1.x:显示自定义名(若有),优先于自动派生的 title。
    if let Ok(Some(name)) = read_session_name(&base, tid) {
        println!("Name:    {name}");
    }
    println!("Path:    {}", path.display());

    // 读前 20 行:SessionMeta + 消息/压缩预览。
    let content = std::fs::read_to_string(&path).context("read session jsonl")?;

    // v1.x:统一统计(活跃 + 轮转文件全扫)。旧 session 无 TokenCount →
    // 全 0 / None,不破坏展示。
    let stats = session_stats(&base, tid);

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
                if let Some(s) = &stats {
                    print_stats_summary(s);
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
        if let Some(s) = &stats {
            print_stats_summary(s);
        }
    }
    // v1.x:per-model 用量表(每个模型的使用量)。
    if let Some(s) = &stats
        && !s.per_model.is_empty()
    {
        println!();
        print_model_usage_table(&s.per_model);
    }
    if msg_count == 0 {
        println!("(no message records in this session)");
    }
    Ok(())
}

/// `reflect session stats [--workspace W]` —— 跨会话按模型汇总 token 用量
/// (v1.x 模型级统计的全局视图:「每个模型的使用量」)。
///
/// - 不带 `--workspace`:统计 base 下全部会话(`all_model_usage`);
/// - 带 `--workspace`:只统计 `SessionMeta.workspace` 匹配的会话
///   (逐会话 `session_stats` 后按模型合并)。
pub fn stats(workspace: Option<&str>) -> anyhow::Result<()> {
    let base = default_base();
    let usage: Vec<ModelUsage> = match workspace {
        Some(ws) => {
            let sessions = list_sessions(&base).context("list_sessions")?;
            let scoped: Vec<_> = sessions
                .into_iter()
                .filter(|s| s.workspace.as_deref() == Some(ws))
                .collect();
            if scoped.is_empty() {
                println!(
                    "(no sessions found for workspace {ws} under {})",
                    base.display()
                );
                return Ok(());
            }
            // 逐会话统计后按模型合并(CLI 侧合并已聚合的数据,不重复扫文件)。
            let mut merged: Vec<ModelUsage> = Vec::new();
            for s in &scoped {
                if let Some(st) = session_stats(&base, s.session_id) {
                    merge_model_usage(&mut merged, st.per_model);
                }
            }
            merged.sort_by(|a, b| {
                b.total_tokens
                    .cmp(&a.total_tokens)
                    .then_with(|| a.model.cmp(&b.model))
            });
            println!(
                "Model usage across {} session(s) in workspace {ws}:",
                scoped.len()
            );
            merged
        }
        None => {
            let all = all_model_usage(&base);
            if all.is_empty() {
                println!("(no token usage recorded under {})", base.display());
                return Ok(());
            }
            let n = list_sessions(&base)?.len();
            println!("Model usage across {n} session(s):");
            all
        }
    };
    print_model_usage_table(&usage);
    // 总计行。
    let total_calls: u64 = usage.iter().map(|m| m.calls).sum();
    let total_in: u64 = usage.iter().map(|m| m.input_tokens).sum();
    let total_out: u64 = usage.iter().map(|m| m.output_tokens).sum();
    let total_cached: u64 = usage.iter().map(|m| m.cached_tokens).sum();
    let total_tokens: u64 = usage.iter().map(|m| m.total_tokens).sum();
    let total_cost: Option<f64> = {
        let mut sum = 0.0;
        let mut present = false;
        for m in &usage {
            if let Some(c) = m.cost_usd {
                sum += c;
                present = true;
            }
        }
        present.then_some(sum)
    };
    println!(
        "{:<40}  {:>6}  {:>12}  {:>12}  {:>12}  {:>14}  {:>10}",
        "TOTAL",
        total_calls,
        format_with_thousands(&total_in.to_string()),
        format_with_thousands(&total_out.to_string()),
        format_with_thousands(&total_cached.to_string()),
        format_with_thousands(&total_tokens.to_string()),
        match total_cost {
            Some(c) => format!("${:.2}", c),
            None => String::from("--"),
        },
    );
    Ok(())
}

/// 打印会话级统计摘要(Tokens / Cost / 工具调用 / 压缩 / 缓存命中)。
fn print_stats_summary(s: &SessionStats) {
    println!(
        "Tokens:  {} (in {} / out {})",
        format_with_thousands(&s.total_tokens.to_string()),
        format_with_thousands(&s.input_tokens.to_string()),
        format_with_thousands(&s.output_tokens.to_string()),
    );
    if let Some(c) = s.cost_usd {
        println!("Cost:    ${:.2}", c);
    }
    if s.tool_calls > 0 || s.compactions > 0 {
        println!(
            "Tools:   {} call(s), {} compaction(s)",
            s.tool_calls, s.compactions
        );
    }
    // v1.x:压缩节省的 token(before 之和 - after 之和;旧记录无字段 → 0)。
    let saved = s.compaction_tokens_saved();
    if saved > 0 {
        println!(
            "Compact: saved {} token(s) ({} → {})",
            format_with_thousands(&saved.to_string()),
            format_with_thousands(&s.compaction_before_tokens.to_string()),
            format_with_thousands(&s.compaction_after_tokens.to_string()),
        );
    }
    // v1.x:记忆读写审计计数(无记录时不显示)。
    if s.memory_reads > 0 || s.memory_writes > 0 {
        println!(
            "Memory:  {} read(s), {} write(s)",
            s.memory_reads, s.memory_writes
        );
    }
    if let Some(rate) = s.cache_hit_rate()
        && rate > 0.0
    {
        println!(
            "Cache:   {:.1}% hit ({} cached)",
            rate * 100.0,
            s.cached_tokens
        );
    }
}

/// 打印 per-model 用量表(列宽与 `stats` 的总计行对齐)。
fn print_model_usage_table(usage: &[ModelUsage]) {
    println!("Per-model usage:");
    println!(
        "  {:<38}  {:>6}  {:>12}  {:>12}  {:>12}  {:>14}  {:>10}",
        "model", "calls", "input", "output", "cached", "total", "cost"
    );
    for m in usage {
        let model = truncate(&m.model, 38);
        let cost = match m.cost_usd {
            Some(c) => format!("${:.2}", c),
            None => String::from("--"),
        };
        println!(
            "  {:<38}  {:>6}  {:>12}  {:>12}  {:>12}  {:>14}  {:>10}",
            model,
            m.calls,
            format_with_thousands(&m.input_tokens.to_string()),
            format_with_thousands(&m.output_tokens.to_string()),
            format_with_thousands(&m.cached_tokens.to_string()),
            format_with_thousands(&m.total_tokens.to_string()),
            cost,
        );
    }
}

/// 把 `src` 的 per-model 用量合并进 `dst`(按 model 键累加)。
/// `stats --workspace` 的逐会话合并用;同模型跨会话累加。
fn merge_model_usage(dst: &mut Vec<ModelUsage>, src: Vec<ModelUsage>) {
    for m in src {
        match dst.iter_mut().find(|d| d.model == m.model) {
            Some(d) => {
                d.calls += m.calls;
                d.input_tokens += m.input_tokens;
                d.output_tokens += m.output_tokens;
                d.cached_tokens += m.cached_tokens;
                d.cache_write_tokens += m.cache_write_tokens;
                d.total_tokens += m.total_tokens;
                // cost 存在性:任一带价即带价,求和。
                match (d.cost_usd, m.cost_usd) {
                    (Some(a), Some(b)) => d.cost_usd = Some(a + b),
                    (None, Some(b)) => d.cost_usd = Some(b),
                    (a, None) => d.cost_usd = a,
                }
            }
            None => dst.push(m),
        }
    }
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

/// `reflect session search <query>` —— 跨会话全文搜索(v1.5 新功能)。
///
/// 在全部本地 session 的消息内容里找包含 `query`(大小写不敏感子串)的
/// 记录,按「最近活跃优先」输出命中摘要。输出字段:session id 前缀
/// (可直接喂给 `reflect exec --resume` / `session show`)、角色、摘要。
/// `--json` 以 JSON 行输出,便于脚本消费。
pub fn search(query: &str, limit: usize, json: bool) -> anyhow::Result<()> {
    let base = default_base();
    let hits = reflect_rollout::search_sessions(
        &base,
        query,
        &reflect_rollout::SearchOptions {
            max_hits: limit.max(1),
            ..Default::default()
        },
    )
    .context("search sessions")?;

    if hits.is_empty() {
        println!("(no matches for {query:?} under {})", base.display());
        return Ok(());
    }

    if json {
        for h in &hits {
            let obj = serde_json::json!({
                "session_id": h.session_id.map(|s| s.to_string()),
                "file": h.file.display().to_string(),
                "line_no": h.line_no,
                "role": h.role,
                "snippet": h.snippet,
            });
            println!("{obj}");
        }
        return Ok(());
    }

    println!(
        "{:<10}  {:<10}  snippet",
        "session", "role"
    );
    for h in &hits {
        // session id 取前 8 位前缀(resume / show 均支持前缀定位)。
        let id_prefix: String = h
            .session_id
            .map(|s| s.to_string()[..8].to_string())
            .unwrap_or_else(|| "-".into());
        let role = h.role.as_deref().unwrap_or("-");
        println!("{:<10}  {:<10}  {}", id_prefix, role, h.snippet);
    }
    println!("\n{} hit(s);用 `reflect session show <id前缀>` 看详情", hits.len());
    Ok(())
}

/// 解析 `id`(完整 UUID 或前缀)为 session 文件路径。
///
/// 路径解析策略(修复「show/rm 用今天日期拼路径导致历史会话找不到」的 bug):
/// 1. 前缀匹配:从 `list_sessions` 拿到精确 `started_at`,用 `session_path_at`
///    拼 `<base>/YYYY/MM/DD/<id>.jsonl`(日期 = session 创建日,非今天)。
/// 2. 完整 UUID 且在 session 列表中:同上,用真实 `started_at`。
/// 3. 完整 UUID 但不在列表中(如跨天 / started_at 缺失):`find_session_file`
///    全盘扫描兜底。
///
/// 双 ID 兜底(修复「文件名是 thread id、内部 session_id 是另一个 UUID,
/// 按内部 id / 内部 id 前缀定位不到文件」的 bug):
/// - 文件名查找(`session_path_at` / `find_session_file`)未命中时,
///   按首行 `SessionMeta.session_id` 全盘再找一次(`find_file_by_session_id`)。
/// - 内部 id 前缀在 `list_sessions` 0 命中时,回退按**文件名**前缀查找
///   (`find_files_by_name_prefix`),按内部 id 去重后判唯一性。
///
/// 返回 `(ThreadId, 路径)`。
fn resolve_session_path(
    base: &std::path::Path,
    id: &str,
) -> anyhow::Result<(ThreadId, std::path::PathBuf)> {
    let resolved = match resolve_session_id(base, id) {
        Ok(r) => r,
        // 内部 session_id 前缀 0 命中:回退按文件名(thread id)前缀查找。
        Err(e) if id.len() != 36 => {
            let files = find_files_by_name_prefix(base, id)?;
            let deduped = dedupe_prefix_files(&files);
            match deduped.len() {
                0 => return Err(e),
                1 => return Ok(deduped.into_iter().next().expect("len == 1")),
                n => {
                    return Err(anyhow!(
                        "prefix '{id}' matches {n} sessions; please give a longer prefix or full UUID"
                    ));
                }
            }
        }
        Err(e) => return Err(e),
    };
    match resolved {
        // 有 SessionInfo:用真实 started_at 拼路径,失败再兜底扫描。
        Some(info) => {
            let path = session_path_at(base, info.session_id, info.started_at);
            if path.exists() {
                Ok((info.session_id, path))
            } else {
                // 文件名是 thread id,与内部 session_id 可能不同:先按文件名
                // 找,再按首行内部 id 全盘找。
                let found = find_session_file(base, info.session_id)?.or_else(|| {
                    find_file_by_session_id(base, &info.session_id)
                        .ok()
                        .flatten()
                });
                found
                    .ok_or_else(|| anyhow!("session file not found: {}", path.display()))
                    .map(|p| (info.session_id, p))
            }
        }
        // 完整 UUID 但不在列表:文件名查找未命中时按首行内部 id 再找
        // (完整 UUID 既可能是 thread id 也可能是内部 session_id)。
        None => {
            let tid = Uuid::parse_str(id)
                .map(ThreadId)
                .map_err(|e| anyhow!("invalid UUID '{id}': {e}"))?;
            let found = find_session_file(base, tid)?
                .or_else(|| find_file_by_session_id(base, &tid).ok().flatten())
                .ok_or_else(|| anyhow!("session file not found for id: {id}"))?;
            Ok((tid, found))
        }
    }
}

/// 按文件**内部** `session_id` 对前缀命中的文件去重,返回
/// `(内部 session_id, 路径)` 列表。同一 session 的活跃文件与轮转 `.N`
/// 副本只保留活跃文件(找不到活跃文件时保留任一副本)。
///
/// 首行无 `SessionMeta`(writer 崩溃等)时退化为文件名 stem;stem 也非
/// UUID 的候选跳过,避免把无关 jsonl 误当作 session。
fn dedupe_prefix_files(files: &[std::path::PathBuf]) -> Vec<(ThreadId, std::path::PathBuf)> {
    let mut out: Vec<(ThreadId, std::path::PathBuf)> = Vec::new();
    for f in files {
        let sid = first_line_session_id(f).or_else(|| {
            f.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|s| Uuid::parse_str(s).ok())
                .map(ThreadId)
        });
        let Some(sid) = sid else {
            continue;
        };
        let active = f
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
            .map(|stem| !stem.contains('.'))
            .unwrap_or(false);
        match out.iter_mut().find(|(s, _)| *s == sid) {
            Some((_, prev)) if active => *prev = f.clone(),
            None => out.push((sid, f.clone())),
            Some(_) => {}
        }
    }
    out
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

    // ── v1.x:统一统计(替代 CLI 侧复刻的 aggregate_token_counts) ─────────

    /// 构造一行 `TokenCount` JSONL(原始 JSON 字符串,避免测试依赖 chrono
    /// 直接构造 `RolloutRecord::TokenCount { at: DateTime }`)。
    /// `model` / `provider` 可选(v1.x 模型级统计字段)。
    fn token_count_line_json(
        input: u32,
        output: u32,
        total: u32,
        cost_usd: Option<f64>,
        model: Option<&str>,
    ) -> String {
        let cost_field = match cost_usd {
            Some(c) => format!(r#","cost_usd":{c}"#),
            None => String::new(),
        };
        let model_field = match model {
            Some(m) => format!(r#","model":"{m}","provider":"openai""#),
            None => String::new(),
        };
        format!(
            r#"{{"type":"token_count","turn_id":"00000000-0000-0000-0000-00000000000{n}","usage":{{"input_tokens":{input},"output_tokens":{output},"cached_tokens":0,"cache_write_tokens":0,"total_tokens":{total}}},"at":"2026-07-31T00:00:00Z"{cost_field}{model_field}}}"#,
            n = input % 10,
        )
    }

    /// `session_stats` 聚合多条 TokenCount 的 usage 与 cost(含 per-model)。
    #[test]
    fn session_stats_sums_usage_and_cost() {
        use crate::test_home::lock_home;
        let tmp = tmpdir();
        let _guard = lock_home(tmp.path());
        let tid = Uuid::new_v4();
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
        for (i, o, t, c) in [
            (1000u32, 200u32, 1200u32, Some(0.01f64)),
            (200, 50, 250, Some(0.008)),
            (300, 100, 400, Some(0.0054)),
        ] {
            body.push_str(&token_count_line_json(i, o, t, c, Some("openai/gpt-4o")));
            body.push('\n');
        }
        fs::write(&path, body).unwrap();

        let base = default_base();
        let stats = session_stats(&base, ThreadId(tid)).expect("stats");
        assert_eq!(stats.input_tokens, 1500);
        assert_eq!(stats.output_tokens, 350);
        assert_eq!(stats.total_tokens, 1850);
        let cost = stats.cost_usd.expect("cost present");
        assert!((cost - 0.0234).abs() < 1e-9, "cost: {cost}");
        // per-model:三条记录同模型 → 一个条目。
        assert_eq!(stats.per_model.len(), 1);
        assert_eq!(stats.per_model[0].model, "openai/gpt-4o");
        assert_eq!(stats.per_model[0].calls, 3);
    }

    /// 旧 jsonl(无 TokenCount)→ 全 0 / None,向后兼容。
    #[test]
    fn session_stats_zero_for_old_rollout() {
        use crate::test_home::lock_home;
        let tmp = tmpdir();
        let _guard = lock_home(tmp.path());
        let tid = Uuid::new_v4();
        let dir = tmp
            .path()
            .join(".reflect")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("31");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tid}.jsonl"));
        fs::write(
            &path,
            format!("{{\"type\":\"session_meta\",\"session_id\":\"{tid}\",\"model\":\"m\",\"started_at\":\"2026-07-31T12:00:00Z\"}}\n{{\"type\":\"message\",\"role\":\"user\",\"content\":\"hi\"}}\n"),
        )
        .unwrap();

        let base = default_base();
        let stats = session_stats(&base, ThreadId(tid)).expect("stats");
        assert_eq!(stats.input_tokens, 0);
        assert_eq!(stats.output_tokens, 0);
        assert_eq!(stats.total_tokens, 0);
        assert_eq!(stats.cost_usd, None);
        assert!(stats.per_model.is_empty());
    }

    /// `show()` / `ls()` 在含 TokenCount 记录的 session 上不 panic,且
    /// `list_sessions` 聚合出正确的累计值。`show()` / `ls()` 内部用
    /// `default_base()` 读 `$HOME`,故这里用 `lock_home` 把 HOME 指到
    /// tmpdir(stdout 渲染正确性由 `format_with_thousands` 单测覆盖,
    /// 避免引入 stdout-capture 依赖)。
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
        body.push_str(&token_count_line_json(
            1500,
            350,
            1850,
            Some(0.0234),
            Some("openai/gpt-4o"),
        ));
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

    /// `merge_model_usage` 按模型键累加跨会话用量。
    #[test]
    fn merge_model_usage_accumulates_by_model() {
        let mk = |model: &str, calls: u64, total: u64, cost: Option<f64>| ModelUsage {
            model: model.to_string(),
            provider: None,
            calls,
            input_tokens: total,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: total,
            cost_usd: cost,
        };
        let mut dst = vec![mk("openai/gpt-4o", 2, 100, Some(0.01))];
        merge_model_usage(
            &mut dst,
            vec![
                mk("openai/gpt-4o", 3, 200, Some(0.02)),
                mk("anthropic/claude", 1, 500, None),
            ],
        );
        assert_eq!(dst.len(), 2);
        let gpt = dst.iter().find(|m| m.model == "openai/gpt-4o").unwrap();
        assert_eq!(gpt.calls, 5);
        assert_eq!(gpt.total_tokens, 300);
        assert!((gpt.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        let claude = dst.iter().find(|m| m.model == "anthropic/claude").unwrap();
        assert_eq!(claude.calls, 1);
        assert_eq!(claude.cost_usd, None);
    }
}
