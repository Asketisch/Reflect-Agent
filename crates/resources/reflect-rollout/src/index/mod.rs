//! `list_sessions` 会遍历 base 目录下的日期目录树,为每个 JSONL
//! 文件返回一条 [`SessionInfo`]。
//!
//! 每个文件的第一行预期是 `SessionMeta` record;若不是(例如
//! writer 在 flush 前崩溃),该文件会被记 warn 并跳过。
//!
//! v0.2.4 新增 [`list_sessions_with_discussion`]:只列出 JSONL 文件中
//! 包含指定 `discussion_id` 的 `RolloutRecord::DiscussionTranscript`
//! 记录的 session。

use std::collections::HashMap;
use std::path::Path;

use chrono::Utc;
use reflect_protocol::{MessageRole, RolloutRecord, SessionInfo, ThreadId, TurnId};

/// 扫描 `<base>/**/*.jsonl`,为每个 **session** 返回一条
/// [`SessionInfo`](按 `session_id` 去重)。开销低:每个文件只读首行。
///
/// ## 为什么要去重
///
/// 由于 writer 在轮转时会重写 `SessionMeta`(见 `writer.rs` 模块 doc),
/// 每份轮转副本 `<id>.N.jsonl` 也都以 `SessionMeta` 开头。若不去重,
/// 同一个 session 会在每个文件里各出现一次。我们对每个 `session_id`
/// 只保留一条,优先选用活跃文件(无 `.N` 后缀的 `<id>.jsonl` ——
/// 它的正文最新)并使用其 `message_count`。
/// 并列(无活跃文件,只剩轮转副本)时保留 index 最大的那份轮转副本。
pub fn list_sessions(base: &Path) -> std::io::Result<Vec<SessionInfo>> {
    let mut raw: Vec<(SessionInfo, std::path::PathBuf)> = Vec::new();
    if !base.exists() {
        return Ok(Vec::new());
    }
    walk(base, &mut raw)?;
    // 按 session_id 分组,优先选活跃(非轮转)文件。
    let mut by_id: HashMap<ThreadId, (SessionInfo, usize)> = HashMap::new();
    for (info, path) in raw {
        // 优先级:活跃文件(suffix_rank 0)> .N 副本(rank N)。
        // 对轮转副本,值越大越优,这样退回到最近的轮转。
        let rank = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s: &str| {
                let stem = s.strip_suffix(".jsonl")?;
                stem.rsplit_once('.')
                    .and_then(|(_, n): (&str, &str)| n.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let active_rank = usize::MAX;
        let effective_rank = if rank == 0 { active_rank } else { rank };
        by_id
            .entry(info.session_id)
            .and_modify(|(prev, prev_rank)| {
                if effective_rank > *prev_rank {
                    *prev = info.clone();
                    *prev_rank = effective_rank;
                }
            })
            .or_insert((info, effective_rank));
    }
    let mut out: Vec<SessionInfo> = by_id.into_values().map(|(info, _)| info).collect();
    // 按 started_at 倒序,最新在前。
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}

/// v0.4: 把 `-c` / `-r N` 旗标解析成具体 `ThreadId`。
///
/// - `continue_last == true`:返回 `list_sessions` 最新一条;若列表为空 → Err。
/// - `resume_by = Some(n)`:返回第 `n` 条(1-indexed);越界 → Err 含具体计数。
/// - 两者都为 `None` / 互斥 → Err(交给 caller 决定是否要报错)。
///
/// 列表**在调用时立即冻结**:返回的 `ThreadId` 在调用瞬间确定,不随后续新
/// session 写入而漂移。这样调用方可以安全地把 `ThreadId` 灌给 `bootstrap_resume`,
/// 不会因为启动过程中 session 列表变化而错位。
pub fn resolve_session_index(
    base: &Path,
    continue_last: bool,
    resume_by: Option<usize>,
) -> anyhow::Result<ThreadId> {
    // 互斥校验(虽然 clap 已经强制,这里再 defense-in-depth 一层)。
    if continue_last && resume_by.is_some() {
        return Err(anyhow::anyhow!(
            "continue_last and resume_by are mutually exclusive"
        ));
    }
    if !continue_last && resume_by.is_none() {
        return Err(anyhow::anyhow!(
            "resolve_session_index called without -c / -r"
        ));
    }
    let sessions = list_sessions(base)?;
    if sessions.is_empty() {
        return Err(anyhow::anyhow!(
            "no sessions found under {}; nothing to resume",
            base.display()
        ));
    }
    if continue_last {
        return Ok(sessions[0].session_id);
    }
    // resume_by:从 1 开始计数。
    let n = resume_by.expect("validated above");
    if n == 0 {
        return Err(anyhow::anyhow!(
            "resume index must be >= 1 (use -c to pick newest)"
        ));
    }
    if n > sessions.len() {
        return Err(anyhow::anyhow!(
            "no session at index {n}; only {} sessions available",
            sessions.len()
        ));
    }
    Ok(sessions[n - 1].session_id)
}

/// v0.2.4:列出包含指定 `discussion_id` 的 `DiscussionTranscript`
/// 记录的 session。
///
/// 实现:遍历 `<base>/**/*.jsonl` 文件,对每个文件逐行扫描 JSONL record,
/// 一旦匹配 `RolloutRecord::DiscussionTranscript { discussion_id, .. } == given_id`,
/// 即将该文件的 SessionInfo 加入结果。文件可能很大,但 DiscussionTranscript
/// 通常出现在文件尾部,所以这里采取折中策略:一次读整个文件
/// (简单实现,未来可以优化成只读尾部 16 KiB)。
///
/// `discussion_id` 是 UUID 字符串(匹配时 `Uuid::parse_str` 后比较)。
pub fn list_sessions_with_discussion(
    base: &Path,
    discussion_id: &str,
) -> std::io::Result<Vec<SessionInfo>> {
    let mut out = Vec::new();
    if !base.exists() {
        return Ok(out);
    }
    let target = match uuid::Uuid::parse_str(discussion_id) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, "invalid discussion_id uuid");
            return Ok(out);
        }
    };
    walk_with_discussion(base, &target, &mut out)?;
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<(SessionInfo, std::path::PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && let Some(info) = parse_first_session_meta(&path)
        {
            out.push((info, path));
        }
    }
    Ok(())
}

fn walk_with_discussion(
    dir: &Path,
    target: &uuid::Uuid,
    out: &mut Vec<SessionInfo>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_with_discussion(&path, target, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && session_contains_discussion(&path, target)
            && let Some(info) = parse_first_session_meta(&path)
        {
            out.push(info);
        }
    }
    Ok(())
}

fn parse_first_session_meta(path: &Path) -> Option<SessionInfo> {
    let body = std::fs::read_to_string(path).ok()?;
    let first = body.lines().next()?;
    let non_blank_lines = body.lines().filter(|l| !l.trim().is_empty()).count();
    // v1.x:扫描首条 User 消息文本,派生会话标题(零额外 I/O —— body 已在内存)。
    let title = first_user_message_title(&body);
    // v1.x:聚合 TokenCount 记录(token 累计 + cost 求和)。
    // body 已在内存,O(n) 行扫描。空 session / 旧 jsonl → 全 0 / None。
    let (input_tokens, output_tokens, total_tokens, cost_usd) = aggregate_token_counts(&body);

    // Happy path:首行本身就是 SessionMeta。`message_count` 把这一行 meta 减 1。
    if let Ok(record) = serde_json::from_str::<RolloutRecord>(first)
        && let RolloutRecord::SessionMeta {
            session_id,
            model,
            started_at,
        } = record
    {
        return Some(SessionInfo {
            session_id,
            model,
            started_at,
            message_count: non_blank_lines.saturating_sub(1),
            title,
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
        });
    }

    // Recovery path:本文件首行不是 SessionMeta。这种情况发生在:
    // 较早的 rollout 文件(writer 还没有在轮转时重写 SessionMeta),
    // 或者文件本身就是某份轮转的 `.N` 副本。此时去扫描同级轮转副本
    // (`<stem>.1.jsonl` / `.2.jsonl` / `.3.jsonl`) —— 它们之中
    // 至少有一份的首行带原 SessionMeta。不这么做的话,经过轮转
    // 把唯一带 SessionMeta 的文件挤掉之后,长 session 就会从
    // `/session` 中消失。
    // 这里 *本* 文件没有 meta 行,所以每条非空行都是消息 —— 不减 1。
    //
    // 注意:此处 token 聚合只取当前文件。rotated sibling 通常不含 TokenCount
    // (TokenCount 是 append-only 在新文件追加,meta 旋到旧文件),
    // 故不复用 sibling 的累计 —— 保留当前文件的聚合值即可。
    if let Some(meta) = session_meta_from_rotated_sibling(path) {
        return Some(SessionInfo {
            session_id: meta.session_id,
            model: meta.model,
            started_at: meta.started_at,
            message_count: non_blank_lines,
            title,
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
        });
    }

    tracing::warn!("rollout: {} has no SessionMeta first line", path.display());
    None
}

/// v1.x:扫描 JSONL body,聚合所有 `RolloutRecord::TokenCount` 记录的
/// `usage` 与 `cost_usd`。
///
/// 返回 `(input_tokens, output_tokens, total_tokens, cost_usd)`:
/// - token 三项均为 `u64`(`saturating_add` 累加,防溢出);
/// - `cost_usd` 仅当至少一条记录带 `Some(c)` 时为 `Some(sum)`,否则 `None`
///   —— 避免「model 不在 pricing 表的 0」被显示为 `$0.00`。
///
/// `f64` 求和精度足够:每轮 $0.001 量级,1000 次累加误差 ~$1e-13,
/// UI `.2f` 显示无影响。`body` 已在内存,本函数无额外 I/O。
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

/// v1.x:扫描 JSONL body,找第一条 `RolloutRecord::Message { role: User, .. }`
/// 的文本内容,用 [`reflect_protocol::derive_title`] 派生标题。
///
/// 实现细节:
/// - `body` 已由调用方读入内存,本函数仅迭代行,无额外 I/O。
/// - `content: serde_json::Value` 优先取字符串(`as_str()`);非字符串
///   (数字 / 数组 / 对象)回退到 `to_string()` 并去掉首尾 `"`。
/// - 解析失败的行静默跳过(与 `replay` 一致,不刷屏)。
/// - 找到首条即返回(短路与);无任何 User 消息 → `None`。
fn first_user_message_title(body: &str) -> Option<String> {
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<RolloutRecord>(line) else {
            continue;
        };
        if let RolloutRecord::Message {
            role: MessageRole::User,
            content,
            ..
        } = record
        {
            let text = match &content {
                serde_json::Value::String(s) => s.clone(),
                other => {
                    // 非字符串:去掉 serde_json::to_string 加的引号。
                    let raw = other.to_string();
                    raw.trim_matches('"').to_string()
                }
            };
            return reflect_protocol::derive_title(&text);
        }
    }
    None
}

/// 扫描 `path` 的同级轮转副本(`.1.jsonl` 到 `.MAX_ROTATED_FILES.jsonl`),
/// 返回第一份带 SessionMeta 的内容。每份轮转文件只检查首行,首个命中
/// 即胜出。若没有同级文件首行是 SessionMeta(或 `path` 没有 stem),
/// 返回 `None`。
///
/// 调用方已持本文件正文;每个同级文件至多读一行,故总开销很低
/// (≤ 3 行 read)。
fn session_meta_from_rotated_sibling(path: &Path) -> Option<SessionMetaFields> {
    use crate::types::MAX_ROTATED_FILES;
    let stem = path.file_name()?.to_str()?;
    let stem = stem.strip_suffix(".jsonl")?;
    let parent = path.parent()?;
    for n in 1..=MAX_ROTATED_FILES {
        let sibling = parent.join(format!("{stem}.{n}.jsonl"));
        let Ok(mut f) = std::fs::File::open(&sibling) else {
            continue;
        };
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(&mut f);
        let mut first_line = String::new();
        if reader.read_line(&mut first_line).ok()? == 0 {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<RolloutRecord>(first_line.trim())
            && let RolloutRecord::SessionMeta {
                session_id,
                model,
                started_at,
            } = record
        {
            return Some(SessionMetaFields {
                session_id,
                model,
                started_at,
            });
        }
    }
    None
}

/// 由 [`session_meta_from_rotated_sibling`] 抽取的扁平字段。
struct SessionMetaFields {
    session_id: ThreadId,
    model: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

fn session_contains_discussion(path: &Path, target: &uuid::Uuid) -> bool {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(RolloutRecord::DiscussionTranscript { discussion_id, .. }) =
            serde_json::from_str::<RolloutRecord>(line)
            && &discussion_id == target
        {
            return true;
        }
    }
    false
}

// ── S5b:rename / fork 真实化 API ────────────────────────────────────────
//
// 存储约定:session 的人可读 name 写到 `<base>/_names/<id>.name`
// (`_names` 是下划线前缀的隐藏子目录,`walk` 只挑选 `.jsonl`,
// 自动忽略)。Fork record 直接 append 到 parent session JSONL 末尾
// (对齐 `replay` 的 append-only 假设)。

/// 递归扫描 `base` 找到 `id` 对应的 JSONL 文件路径。文件第一行必须是
/// `SessionMeta { session_id: id, .. }`,否则跳过(防御错位文件)。
///
/// `Some(path)` 找到,`None` 找不到(包括 base 不存在)。
/// **不**缓存结果 —— TUI 启动后 session 文件可能新增,
/// 因此每次调用都重新 walk。
pub fn find_session_path(base: &Path, id: ThreadId) -> Option<std::path::PathBuf> {
    find_session_path_inner(base, id)
}

fn find_session_path_inner(dir: &Path, id: ThreadId) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_session_path_inner(&path, id) {
                return Some(found);
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && let Some(info) = parse_first_session_meta(&path)
            && info.session_id == id
        {
            return Some(path);
        }
    }
    None
}

/// 把 `id` 对应 session 改名为 `new_name`。存储到 `<base>/_names/<id>.name`
/// 的 plain text 文件(UTF-8)。重复调用即覆盖(支持"再改一次")。
///
/// `new_name.trim().is_empty()` → `Err(InvalidInput)`,避免写出空文件
/// 让后续 read 误判为"未命名"。
pub fn rename_session(base: &Path, id: ThreadId, new_name: &str) -> std::io::Result<()> {
    if new_name.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rename: new name cannot be empty",
        ));
    }
    let names_dir = base.join("_names");
    std::fs::create_dir_all(&names_dir)?;
    let path = names_dir.join(format!("{id}.name"));
    std::fs::write(path, new_name.as_bytes())
}

/// 读取 `id` 的已命名 session 名。`None` = 未命名(无 .name 文件);
/// `Some(name)` = 已命名。trim 去除尾随换行(`std::fs::write` 不会加
/// `\n`,但 `read_to_string` 也不会;trim 是 defense-in-depth)。
pub fn read_session_name(base: &Path, id: ThreadId) -> std::io::Result<Option<String>> {
    let path = base.join("_names").join(format!("{id}.name"));
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 把 `RolloutRecord::Fork` 写到 `parent_id` 对应 session 的 JSONL 末尾,
/// 返回新 session 的 ThreadId。
///
/// 行为细节:
/// - 找不到 parent 文件 → `Err(NotFound)`,错误信息含 UUID,
///   方便 TUI Pill 报错。
/// - `branch_name` 直接写入 record 字段(`Vec<String>` 的
///   participants 留给 v2.x)。
/// - 用 `OpenOptions::create(true).append(true)` —— JSONL 是
///   append-only,重复 fork 不会相互覆盖。
/// - 失败时不回滚已写入的字节(JSONL 单行写入要么完整成功,
///   要么是 NotFound,不存在中间态)。
pub fn write_fork_record(
    base: &Path,
    parent_id: ThreadId,
    branch_name: &str,
) -> std::io::Result<ThreadId> {
    use std::io::Write;

    let parent_path = find_session_path(base, parent_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "parent session {parent_id} not found under {}",
                base.display()
            ),
        )
    })?;
    let new_id = ThreadId::new();
    let record = RolloutRecord::Fork {
        parent_session_id: parent_id,
        branch_name: branch_name.to_string(),
    };
    let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&parent_path)?;
    writeln!(f, "{line}")?;
    Ok(new_id)
}

/// v1.2 P2:把父会话「截止 fork 点」的完整历史**原样复制**进一个新建的
/// 子会话 JSONL,并在父子两端标记 `Fork` 关系。返回子 `ThreadId`。
///
/// 与 [`write_fork_record`] 的区别:`write_fork_record` 只往父 JSONL append
/// 一条 marker、返回一个**没有文件**的新 id;`fork_with_history` 真正创建
/// 子 JSONL 并把历史写进去,使得 `reflect --resume <child_id>` 能在子会话里
/// 看到 fork 点之前的完整对话。
///
/// ## v1.2 P2 改造要点(原样复制)
///
/// 旧版签名要求调用方传入
/// `up_to_messages: &[(MessageRole, String)]`,只能表达纯文本,
/// 会丢失 ToolUse / ToolResult / Image 等结构化块;且 user 消息
/// 从未落盘,调用方只能从内存 history 拼凑 —— 实际并不能正确工作。
///
/// 新版直接从父 JSONL 读取原始 `RolloutRecord` 列表,
/// **逐行原样复制**(保留原 `turn_id` + 完整 content),不再做任何
/// 格式转换。这样:
/// - user 消息(v1.2 P2 已落盘)、assistant 完整 ContentBlocks、Compaction
///   摘要、Checkpoint / Rewind marker 全部无损继承;
/// - 与 `bootstrap_resume` 的回放逻辑完全对称(读同样的 records)。
///
/// ## 设计要点
/// - **子文件首行必须是 `SessionMeta { session_id: child_id, .. }`**,
///   否则 `find_session_path` 找不到子文件。父的 SessionMeta 被跳过,
///   用 child 的新 id + 从父 SessionMeta 继承的 model 替换。
/// - `up_to_turn_id`:`None` = 截至父文件末尾全量复制;
///   `Some(tid)` = 截至该 turn(含)。截断按 `Message` / `Compaction`
///   等 record 携带的 `turn_id` 字段判断 —— 遇到 turn_id 匹配的 record
///   后,该 record 及之前的全部保留,之后的丢弃。
/// - **父 JSONL 也 append 一条 `Fork` marker**(血缘追踪)。
/// - 同步读 + 同步写(不依赖 tokio reactor),CLI 可直接调用。
pub fn fork_with_history(
    base: &Path,
    parent_id: ThreadId,
    branch_name: &str,
    up_to_turn_id: Option<&TurnId>,
) -> std::io::Result<ThreadId> {
    use std::io::Write;

    // 1) 校验父会话存在并读取全部原始 records(同步,与
    //    `reader::replay_path` 语义一致:空行跳过,畸形行 skip 不 panic)。
    let parent_path = find_session_path(base, parent_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "parent session {parent_id} not found under {}",
                base.display()
            ),
        )
    })?;
    let parent_content = std::fs::read_to_string(&parent_path)?;
    let parent_records: Vec<RolloutRecord> = parent_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<RolloutRecord>(l).ok())
        .collect();

    // 2) 从父 SessionMeta 提取 model(子会话继承父模型)。
    let parent_model: String = parent_records
        .iter()
        .find_map(|r| match r {
            RolloutRecord::SessionMeta { model, .. } => Some(model.clone()),
            _ => None,
        })
        .unwrap_or_default();

    // 3) 分配子 id + 计算子文件路径(按当前 UTC 日期分桶)。
    let child_id = ThreadId::new();
    let child_path = crate::path::session_path_at(base, child_id, Utc::now());
    if let Some(parent_dir) = child_path.parent() {
        std::fs::create_dir_all(parent_dir)?;
    }

    // 序列化辅助:把 record 转成单行 JSON(不带末尾换行,由 writeln! 补)。
    let to_line = |r: &RolloutRecord| serde_json::to_string(r).map_err(std::io::Error::other);

    // 4) 写子文件:truncate 新建。
    let mut child = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&child_path)?;

    // 4a) 首行:子会话自己的 SessionMeta(用 child_id + 继承的 model)。
    writeln!(
        child,
        "{}",
        to_line(&RolloutRecord::SessionMeta {
            session_id: child_id,
            model: parent_model,
            started_at: Utc::now(),
        })?
    )?;

    // 4b) 原样遍历父 records,复制到子文件。
    //     - 跳过父 SessionMeta(已用 child_id 重写首行)。
    //     - 跳过父 Fork marker(避免血缘混乱,子文件末尾会写自己的 Fork)。
    //     - up_to_turn_id 截断:命中目标 turn 后,继续复制**同 turn_id**
    //       的后续 records(同一 turn 可能有多条:User + Assistant +
    //       TokenCount),直到 turn_id 变化才停止。
    let mut hit_target = false;
    for r in &parent_records {
        match r {
            RolloutRecord::SessionMeta { .. } | RolloutRecord::Fork { .. } => continue,
            _ => {}
        }
        // 提取该 record 携带的 turn_id(若有)。
        let record_turn_id: Option<&TurnId> = match r {
            RolloutRecord::Message { turn_id, .. }
            | RolloutRecord::Compaction { turn_id, .. }
            | RolloutRecord::Checkpoint { turn_id, .. }
            | RolloutRecord::Rewind { turn_id, .. }
            | RolloutRecord::TokenCount { turn_id, .. } => Some(turn_id),
            // v1.x:PlanRequest / PlanReady / PlanRejected / PermissionModeChanged(均为 enum 变体名)
            // 不绑定 turn(plan 生命周期跨 turn),走 None → 不参与 up_to_turn_id
            // 截断,原样复制到子会话。`_` 兜底已覆盖,此处仅为显式说明意图。
            _ => None,
        };
        // 截断判断:若已命中目标 turn 且当前 record 属于不同 turn → 停止。
        if let (Some(target), Some(rt), true) = (up_to_turn_id, record_turn_id, hit_target)
            && rt != target
        {
            break;
        }
        writeln!(child, "{}", to_line(r)?)?;
        // 标记命中:当前 record 的 turn_id == 目标 → 后续同 turn 继续复制。
        if let (Some(target), Some(rt)) = (up_to_turn_id, record_turn_id)
            && rt == target
        {
            hit_target = true;
        }
    }

    // 4c) 子文件末尾:Fork 标记(指向 parent)。
    writeln!(
        child,
        "{}",
        to_line(&RolloutRecord::Fork {
            parent_session_id: parent_id,
            branch_name: branch_name.to_string(),
        })?
    )?;
    child.flush()?;

    // 5) 父文件 append 一条 Fork marker(血缘追踪,镜像 `write_fork_record`)。
    let fork_line = to_line(&RolloutRecord::Fork {
        parent_session_id: parent_id,
        branch_name: branch_name.to_string(),
    })?;
    let mut parent_f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&parent_path)?;
    writeln!(parent_f, "{fork_line}")?;

    Ok(child_id)
}

/// v1.2 P0-3:把一条 `Checkpoint` 记录 append 到 `session_id` 的 JSONL。
///
/// 镜像 [`write_fork_record`] 的 append-only 写法:`find_session_path`
/// 定位文件 → `serde_json` 序列化 → append 一行。失败时返回 `io::Error`
/// (NotFound 表示 session 不存在)。
///
/// `sha` 是 `git_auto_commit` 后的 HEAD sha;`label` 可选。返回写入的
/// `Checkpoint` 记录(供 `CheckpointTool` 返回给 LLM)。
pub fn write_checkpoint_record(
    base: &Path,
    session_id: ThreadId,
    turn_id: TurnId,
    sha: &str,
    label: Option<&str>,
) -> std::io::Result<RolloutRecord> {
    use std::io::Write;
    let path = find_session_path(base, session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found under {}", base.display()),
        )
    })?;
    let record = RolloutRecord::Checkpoint {
        turn_id,
        sha: sha.to_string(),
        label: label.map(|s| s.to_string()),
        created_at: Utc::now(),
    };
    let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(record)
}

/// v1.2 P0-3:把一条 `Rewind` 记录 append 到 `session_id` 的 JSONL。
///
/// `target_sha` 是回退到的 checkpoint sha;`from_sha` 是回退前的 HEAD
/// sha(便于审计 / 再次前进)。返回写入的 `Rewind` 记录。
pub fn write_rewind_record(
    base: &Path,
    session_id: ThreadId,
    turn_id: TurnId,
    target_sha: &str,
    from_sha: &str,
) -> std::io::Result<RolloutRecord> {
    use std::io::Write;
    let path = find_session_path(base, session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found under {}", base.display()),
        )
    })?;
    let record = RolloutRecord::Rewind {
        turn_id,
        target_sha: target_sha.to_string(),
        from_sha: from_sha.to_string(),
        at: Utc::now(),
    };
    let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(record)
}

/// v1.2 P0-3:读 `session_id` 的 JSONL,返回所有 `Checkpoint` 记录
/// (按 created_at 升序)。`CheckpointTool` 的 `list` action 用。
pub fn list_checkpoints(base: &Path, session_id: ThreadId) -> std::io::Result<Vec<RolloutRecord>> {
    let Some(path) = find_session_path(base, session_id) else {
        return Ok(Vec::new());
    };
    let body = std::fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<RolloutRecord>(line)
            && matches!(rec, RolloutRecord::Checkpoint { .. })
        {
            out.push(rec);
        }
    }
    Ok(out)
}

/// A3:`/rewind-files` —— 读 `session_id` 的 JSONL,找到 `turn_id`
/// 对应 checkpoint 的 sha。匹配规则:
/// - 若存在 `Checkpoint.turn_id == target` 的精确记录,返回其 sha;
/// - 否则返回 `created_at` 最大的(即最近的)checkpoint 的 sha
///   —— 作为「回退到最近一次快照」的合理默认。
///   若没有任何 checkpoint → `None`。
pub fn find_checkpoint_for_turn(
    base: &Path,
    session_id: ThreadId,
    target: &reflect_protocol::TurnId,
) -> Option<String> {
    let records = list_checkpoints(base, session_id).ok()?;
    let exact = records.iter().rev().find_map(|r| match r {
        RolloutRecord::Checkpoint { turn_id, sha, .. } if turn_id == target => Some(sha.clone()),
        _ => None,
    });
    if exact.is_some() {
        return exact;
    }
    // 兜底:最近的 checkpoint(list_checkpoints 按 created_at 升序)。
    records.iter().rev().find_map(|r| match r {
        RolloutRecord::Checkpoint { sha, .. } => Some(sha.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests;
