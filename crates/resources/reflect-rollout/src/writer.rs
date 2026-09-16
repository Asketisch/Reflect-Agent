//! 按大小轮转的 JSONL rollout writer。
//!
//! 一个 [`JsonlRolloutWriter`] 实例绑定一个 [`ThreadId`],把记录写到
//! `<base>/YYYY/MM/DD/<thread_id>.jsonl`。当文件超过
//! [`crate::types::ROTATE_AFTER_BYTES`] 时触发轮转:
//!
//! 1. 关闭当前文件。
//! 2. 移动 `<id>.jsonl → <id>.1.jsonl`、
//!    `<id>.1.jsonl → <id>.2.jsonl`、
//!    `<id>.2.jsonl → <id>.3.jsonl`。
//! 3. 删除 `<id>.3.jsonl`(最多保留 [`crate::types::MAX_ROTATED_FILES`]
//!    份轮转副本)。
//! 4. 重新打开一份新的 `<id>.jsonl`,并**重写 `SessionMeta` 行**。
//!
//! # 为什么轮转时要重写 `SessionMeta`
//!
//! session 索引(`crate::index::parse_first_session_meta`)要求
//! 每个 `<id>[.N].jsonl` 文件的第一行必须是 `SessionMeta`。`SessionMeta`
//! 在 thread 生命周期里通常由 `submission_loop` 一次性产出;
//! 若不在轮转时重写,经过 `MAX_ROTATED_FILES + 1` 次轮转后
//! 唯一带 `SessionMeta` 的文件就会被删,整个 session 从
//! `/session` 中消失。为维持「首行 = SessionMeta」不变量,
//! writer 在首次见到 `SessionMeta` 时缓存之,每份轮转新文件
//! 都把这条 meta 重新写到首行。
//!
//! 并发:用 `parking_lot::Mutex` 串行化所有写操作,确保 writer
//! 满足 `Send + Sync`,可以安全地放在 `Arc` 后面共享。

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::path::session_path_at;
use crate::redact::serialize_redacted;
use crate::types::{MAX_ROTATED_FILES, ROTATE_AFTER_BYTES};
use chrono::{DateTime, Utc};
use reflect_protocol::{RolloutRecord, RolloutRecorder, SessionInfo, ThreadId, TurnId};

/// 缓存的 `SessionMeta` 快照,用于轮转时重写。`session_id` 直接挂在
/// [`JsonlRolloutWriter`] 上;此处只缓存 `model` + `started_at` 以便
/// `rotate()` 能合成一份 `SessionMeta`,写入新打开文件的首行
/// (不变量含义见模块级 doc)。
///
/// v1.x:增加 `workspace` 缓存 —— 轮转时把同一份 `workspace` 带到新文件首行,
/// 保证索引层 `SessionInfo.workspace` 在轮转后仍可命中。
#[derive(Clone)]
struct CachedSessionMeta {
    model: String,
    started_at: DateTime<Utc>,
    workspace: Option<String>,
}

/// 内部可变状态,由外层 `Mutex` 保护。
struct Inner {
    current_path: Option<PathBuf>,
    /// 当 `current_path` 为 `Some` 时,本字段恒为 `Some`;
    /// 缓冲区在每次 `record()` 调用时都 flush,因此进程崩溃
    /// 至多丢失一条记录。
    writer: Option<BufWriter<File>>,
    current_size: u64,
    /// 在第一条 `SessionMeta` 记录流过 `record()` 时被设置。
    /// `rotate()` 用它把同一份 meta 重写到新文件首行,
    /// 以保证索引的「首行 = SessionMeta」契约。
    /// 在见到 `SessionMeta` 之前为 `None`(此时轮转是 no-op)。
    session_meta: Option<CachedSessionMeta>,
}

/// 持久化 JSONL writer,一个 thread 一份。
pub struct JsonlRolloutWriter {
    base_dir: PathBuf,
    session_id: ThreadId,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for JsonlRolloutWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlRolloutWriter")
            .field("base_dir", &self.base_dir)
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl JsonlRolloutWriter {
    /// 构造一个以 `base_dir` 为根的 writer。第一次 `record()` 调用会
    /// 惰性创建按日期划分的目录。
    pub fn new(base_dir: impl Into<PathBuf>, session_id: ThreadId) -> Self {
        Self {
            base_dir: base_dir.into(),
            session_id,
            inner: Mutex::new(Inner {
                current_path: None,
                writer: None,
                current_size: 0,
                session_meta: None,
            }),
        }
    }

    fn ensure_open(&self, g: &mut Inner) -> std::io::Result<()> {
        if g.current_path.is_some() {
            return Ok(());
        }
        let path = session_path_at(&self.base_dir, self.session_id, chrono::Utc::now());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        g.writer = Some(BufWriter::new(file));
        g.current_path = Some(path);
        g.current_size = size;
        Ok(())
    }

    fn rotate(g: &mut Inner, session_id: ThreadId) -> std::io::Result<()> {
        let Some(current) = g.current_path.take() else {
            return Ok(());
        };
        // Flush + drop writer,让 rename 作用在已关闭的文件上。
        if let Some(mut w) = g.writer.take() {
            w.flush()?;
        }

        let Some(stem) = current
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
        else {
            return Ok(());
        };
        let Some(parent) = current.parent() else {
            return Ok(());
        };

        // 从最大索引向下移动,使每份 .N 拿到前一份 .N-1。
        for n in (1..=MAX_ROTATED_FILES).rev() {
            let src = if n == 1 {
                current.clone()
            } else {
                parent.join(format!(
                    "{}.{}.jsonl",
                    stem.trim_end_matches(".jsonl"),
                    n - 1
                ))
            };
            let dst = parent.join(format!("{}.{}.jsonl", stem.trim_end_matches(".jsonl"), n));
            if src.exists() {
                if dst.exists() {
                    fs::remove_file(&dst)?;
                }
                fs::rename(&src, &dst)?;
            }
        }

        // 在原路径重新打开一份新文件。
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)?;
        g.writer = Some(BufWriter::new(file));
        g.current_path = Some(current);
        g.current_size = 0;

        // 把 `SessionMeta` 重写为第一行,使新打开的文件满足索引的
        // 「首行 = SessionMeta」契约。否则每份轮转文件都会以
        // `Message` 开头,经过足够多次轮转后 session 会从
        // `/session` 中消失。
        if let Some(meta) = g.session_meta.clone() {
            let line = serialize_redacted(&RolloutRecord::SessionMeta {
                session_id,
                model: meta.model,
                started_at: meta.started_at,
                workspace: meta.workspace,
            })?;
            if let Some(w) = g.writer.as_mut() {
                writeln!(w, "{line}")?;
                w.flush()?;
            }
            // 把刚写入的 meta 行大小计入(字符串字节数 + 换行)。
            g.current_size = g.current_size.saturating_add(line.len() as u64 + 1);
        }
        Ok(())
    }
}

#[async_trait]
impl RolloutRecorder for JsonlRolloutWriter {
    async fn record(&self, r: RolloutRecord) -> anyhow::Result<()> {
        let line = serialize_redacted(&r)?;
        let line_bytes = line.len() as u64 + 1; // +1 是换行符

        let mut g = self.inner.lock();
        // 在首次见到 `SessionMeta` 时缓存之,以便 `rotate()` 能把同一份
        // meta 写到每份轮转文件的首行。只保留第一次出现
        // (引擎每个 thread 只发一次 SessionMeta)。
        if g.session_meta.is_none()
            && let RolloutRecord::SessionMeta {
                model,
                started_at,
                workspace,
                ..
            } = &r
        {
            g.session_meta = Some(CachedSessionMeta {
                model: model.clone(),
                started_at: *started_at,
                workspace: workspace.clone(),
            });
        }

        Self::ensure_open(self, &mut g)?;
        if let Some(w) = g.writer.as_mut() {
            writeln!(w, "{}", line)?;
            w.flush()?;
        }
        g.current_size = g.current_size.saturating_add(line_bytes);

        if g.current_size >= ROTATE_AFTER_BYTES {
            Self::rotate(&mut g, self.session_id)?;
        }
        Ok(())
    }

    async fn replay(&self, session_id: ThreadId) -> anyhow::Result<Vec<RolloutRecord>> {
        let path = if session_id != self.session_id {
            session_path_at(&self.base_dir, session_id, chrono::Utc::now())
        } else {
            let g = self.inner.lock();
            let Some(path) = g.current_path.clone() else {
                // 从未写入 —— 返回空。
                drop(g);
                return Ok(Vec::new());
            };
            drop(g);
            path
        };
        crate::reader::replay_path(&path).await
    }

    async fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        crate::index::list_sessions(&self.base_dir).map_err(anyhow::Error::from)
    }

    /// 破坏性回退:`to_turn_id`(含)及其后的记录全部丢弃,只保留之前的。
    /// `None` = 丢弃最后一个 turn。
    ///
    /// # 流程(全在 `inner` mutex 内,同步 IO)
    /// 1. 读当前活跃文件全文(`current_path`),逐行解析定位截断行。
    /// 2. 未找到目标 turn → no-op,返回 0。
    /// 3. **备份**:复制全文到 `<id>.rewind-<unix_ts>.bak`(同目录,可恢复)。
    /// 4. 原子 rewrite:写 `.tmp`(SessionMeta + 截断行之前)→ `fs::rename` 覆盖。
    /// 5. 重开 `BufWriter`(append 模式),更新 `current_size`,保留 `session_meta` 缓存。
    /// 6. 返回被丢弃的 `Message` 记录数。
    ///
    /// 仅作用于当前活跃文件;已 rotate 的 `.N` 文件不处理
    /// (罕见边界,/fork 是替代)。
    async fn truncate_after(&self, to_turn_id: Option<&TurnId>) -> anyhow::Result<usize> {
        let mut g = self.inner.lock();
        Self::ensure_open(self, &mut g)?;
        Self::truncate_inner(&mut g, to_turn_id)
    }
}

impl JsonlRolloutWriter {
    /// `truncate_after` 的同步核心,持有 `inner` 锁的调用方使用。
    /// 解析当前活跃文件,定位 `to_turn_id`(或 None = 最后一个 turn)所在行,
    /// 备份 + 原子 rewrite 到该行之前(不含)。返回丢弃的 `Message` 记录数。
    fn truncate_inner(g: &mut Inner, to_turn_id: Option<&TurnId>) -> anyhow::Result<usize> {
        let path = g
            .current_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("truncate_after: no active rollout file"))?;

        // 全文读 + 逐行解析,记录每行的 (字节长度, turn_id 若有, 是否 Message)。
        let content = std::fs::read_to_string(&path)?;
        if content.is_empty() {
            return Ok(0);
        }

        // 每个 entry:(line_start_byte, line_str, turn_id, is_message)。
        let mut lines: Vec<(usize, &str, Option<TurnId>, bool)> = Vec::new();
        let mut offset = 0usize;
        for line in content.split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n');
            let (turn_id, is_message) = parse_line_meta(trimmed);
            lines.push((offset, trimmed, turn_id, is_message));
            offset += line.len();
        }

        // 定位截断行索引(该行及其后全部丢弃)。
        let cut_idx = match locate_cut(&lines, to_turn_id) {
            Some(i) => i,
            None => return Ok(0), // 未找到目标 turn,no-op。
        };

        // 备份:整文件复制到 <id>.rewind-<ts>.bak(可恢复)。
        write_backup(&path, &content)?;

        // 保留 cut_idx 之前的行 + 结尾换行。
        let mut kept = String::new();
        let mut dropped_messages = 0usize;
        for (i, (_, line, _, is_message)) in lines.iter().enumerate() {
            if i < cut_idx {
                kept.push_str(line);
                kept.push('\n');
            } else if *is_message {
                dropped_messages += 1;
            }
        }

        // 原子 rewrite:写 `.tmp` → 用 `fs::rename` 覆盖
        // (匹配 reflect-task/reflect-plugin 的约定)。
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &kept)?;
        std::fs::rename(&tmp, &path)?;

        // 重开 BufWriter(append 模式),让后续 record() 继续追加;
        // 保留 session_meta 缓存。
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Some(mut w) = g.writer.take() {
            let _ = w.flush();
        }
        g.writer = Some(BufWriter::new(file));
        g.current_size = kept.len() as u64;

        Ok(dropped_messages)
    }
}

/// 解析一行 JSONL,返回 (该行关联的 turn_id 若有, 是否为 Message 记录)。
/// turn_id 来自 Message/Compaction/Checkpoint/Rewind 变体。
fn parse_line_meta(line: &str) -> (Option<TurnId>, bool) {
    // 用 serde_json::Value 解包,避免对全枚举 from_str 的
    // 版本/变体耦合。
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return (None, false);
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let is_message = ty == "message";
    let turn_id = v.get("turn_id").and_then(|t| t.as_str()).and_then(|s| {
        // 解析失败(malformed)→ None,与 reader 的宽容策略一致。
        TurnId::parse_str(s).ok()
    });
    (turn_id, is_message)
}

/// 在已解析的行列表里定位截断行索引。
/// `Some(tid)` → 第一行 turn_id == tid 的索引。
/// `None` → 最后一个有 turn_id 的 turn 的「首行」索引(丢弃整个最后 turn)。
fn locate_cut(
    lines: &[(usize, &str, Option<TurnId>, bool)],
    to_turn_id: Option<&TurnId>,
) -> Option<usize> {
    match to_turn_id {
        Some(target) => lines
            .iter()
            .position(|(_, _, tid, _)| *tid == Some(*target)),
        None => {
            // 找最后一个 turn_id 的值,再回到该 turn 在文件中的第一次出现。
            let last_tid = lines.iter().rev().find_map(|(_, _, tid, _)| *tid)?;
            lines
                .iter()
                .position(|(_, _, tid, _)| *tid == Some(last_tid))
        }
    }
}

/// 把截断前的全文复制到 `<path>.rewind-<unix_ts>.bak`(同目录),可恢复。
/// 失败不致命:warn 后继续(用户已通过原子 rewrite 得到一致性保证)。
fn write_backup(path: &Path, content: &str) -> anyhow::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut bak = path.to_path_buf();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    bak.set_file_name(format!("{stem}.rewind-{ts}.bak"));
    // 极端情况下同秒重复 rewind:加 `-2` / `-3` 直到不冲突。
    let mut n = 2;
    while bak.exists() {
        bak.set_file_name(format!("{stem}.rewind-{ts}-{n}.bak"));
        n += 1;
    }
    std::fs::write(&bak, content)?;
    tracing::debug!("rollout rewind backup written: {}", bak.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{MessageRole, TurnId};
    use tempfile::tempdir;

    fn big_record(n: usize) -> RolloutRecord {
        RolloutRecord::message(
            TurnId::new(),
            MessageRole::Assistant,
            serde_json::json!({"i": n, "blob": "x".repeat(2048)}),
        )
    }

    #[tokio::test]
    async fn append_writes_single_line() {
        let dir = tempdir().unwrap();
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.path(), sid);
        w.record(RolloutRecord::session_meta(sid, "openai/gpt-4o"))
            .await
            .unwrap();
        w.record(big_record(1)).await.unwrap();

        let path = session_path_at(dir.path(), sid, chrono::Utc::now());
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "got lines: {lines:?}");
        // 首行是 SessionMeta
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["type"], "session_meta");
    }

    #[tokio::test]
    async fn rotates_after_threshold() {
        let dir = tempdir().unwrap();
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.path(), sid);
        // 每条记录约 2 KiB;200 条记录 ≈ 400 KiB,足以触发 1 次以上轮转。
        for i in 0..200 {
            w.record(big_record(i)).await.unwrap();
        }
        // 轮转出的文件位于 `.1.jsonl`
        let rotated = dir
            .path()
            .join(".")
            .join("..")
            .canonicalize()
            .unwrap()
            .join("2026");
        // 更简单:遍历日期目录找任一 `.1.jsonl`
        let mut found_rotated = false;
        for entry in walkdir(dir.path()) {
            if entry.ends_with(".1.jsonl") {
                found_rotated = true;
                break;
            }
        }
        assert!(found_rotated, "expected a rotated .1.jsonl file");
        // 抑制未使用警告。
        let _ = rotated;
    }

    #[tokio::test]
    async fn caps_rotated_files_at_max() {
        let dir = tempdir().unwrap();
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.path(), sid);
        // 强制 5 次轮转:共写 5 × ROTATE_AFTER_BYTES 大小的数据。
        for i in 0..(5 * 200) {
            w.record(big_record(i)).await.unwrap();
        }
        let mut indices = Vec::new();
        for entry in walkdir(dir.path()) {
            if let Some(name) = std::path::Path::new(&entry)
                .file_name()
                .and_then(|s| s.to_str())
                && let Some(rest) = name.strip_prefix(&format!("{sid}."))
                && let Some(num) = rest.strip_suffix(".jsonl")
            {
                indices.push(num.to_string());
            }
        }
        let max_idx: usize = indices
            .iter()
            .filter_map(|s| s.parse().ok())
            .max()
            .unwrap_or(0);
        assert!(
            max_idx <= MAX_ROTATED_FILES,
            "max rotated index {max_idx} > MAX_ROTATED_FILES {MAX_ROTATED_FILES}"
        );
    }

    /// Bug fix:轮转后,当前活跃文件和每份轮转出的 `.N` 副本都必须
    /// 以 `SessionMeta`(同 `session_id`)开头。修复前,轮转会
    /// 删除唯一带 `SessionMeta` 的文件并重新打开一个无 meta 的活跃
    /// 文件,导致 session 从 `/session` 中消失。
    #[tokio::test]
    async fn rotation_keeps_session_meta_in_each_file() {
        let dir = tempdir().unwrap();
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.path(), sid);
        // 先植入 meta(此时被缓存),再强制多次轮转。
        w.record(RolloutRecord::session_meta(sid, "openai/gpt-4o"))
            .await
            .unwrap();
        for i in 0..(5 * 200) {
            w.record(big_record(i)).await.unwrap();
        }

        let mut checked = 0;
        for entry in walkdir(dir.path()) {
            let path = std::path::Path::new(&entry);
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let body = std::fs::read_to_string(path).unwrap();
            let first = body.lines().next().expect("non-empty file");
            let v: serde_json::Value = serde_json::from_str(first).unwrap();
            assert_eq!(
                v["type"],
                "session_meta",
                "{} first line is not SessionMeta",
                path.display()
            );
            assert_eq!(
                v["session_id"].as_str().unwrap(),
                sid.to_string(),
                "{} SessionMeta session_id mismatch",
                path.display()
            );
            checked += 1;
        }
        assert!(checked >= 2, "expected ≥2 jsonl files, checked {checked}");
    }

    #[tokio::test]
    async fn redaction_applied_on_write() {
        let dir = tempdir().unwrap();
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.path(), sid);
        w.record(RolloutRecord::message(
            TurnId::new(),
            MessageRole::User,
            serde_json::json!("x".repeat(20_000)),
        ))
        .await
        .unwrap();
        let path = session_path_at(dir.path(), sid, chrono::Utc::now());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[redacted]"), "missing marker in body");
        // 文件体应明显小于原始 20KB。
        assert!(body.len() < 18_000, "body too large: {}", body.len());
    }

    // 小 helper:遍历一层日期目录,把全部路径收集成字符串。
    fn walkdir(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(p: &Path, out: &mut Vec<String>) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else {
                        out.push(path.to_string_lossy().into_owned());
                    }
                }
            }
        }
        walk(root, &mut out);
        out
    }

    // ── truncate_after 测试(批次二十二)──────────────────────────────────

    /// 写一个含多 turn 的会话:SessionMeta + 3 个 user/assistant 对。
    /// 返回各 Message 的 turn_id 供测试定位截断点。
    async fn seed_three_turns(
        dir: &Path,
    ) -> (
        JsonlRolloutWriter,
        ThreadId,
        Vec<TurnId>, // 每个 Message 的 turn_id,共 6 条
    ) {
        let sid = ThreadId::new();
        let w = JsonlRolloutWriter::new(dir.to_path_buf(), sid);
        w.record(RolloutRecord::session_meta(sid, "openai/gpt-4o"))
            .await
            .unwrap();
        let mut tids = Vec::new();
        for i in 0..6 {
            let tid = TurnId::new();
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            w.record(RolloutRecord::message(
                tid,
                role,
                serde_json::json!(format!("msg-{i}")),
            ))
            .await
            .unwrap();
            tids.push(tid);
        }
        (w, sid, tids)
    }

    #[tokio::test]
    async fn truncate_after_specific_turn_drops_turn_and_after() {
        let dir = tempdir().unwrap();
        let (w, sid, tids) = seed_three_turns(dir.path()).await;
        // 截断到 tids[2](第 3 条 Message)→ 应保留 SessionMeta + tids[0,1],
        // 丢弃 tids[2..6](4 条 Message)。
        let dropped = w.truncate_after(Some(&tids[2])).await.unwrap();
        assert_eq!(dropped, 4, "应丢弃 tids[2..6] 共 4 条 Message");

        let records = w.replay(sid).await.unwrap();
        // SessionMeta + 2 条 Message = 共 3 条记录。
        assert_eq!(records.len(), 3, "got: {records:?}");
        // 第一行仍是 SessionMeta。
        assert!(matches!(records[0], RolloutRecord::SessionMeta { .. }));
        // 剩余 Message 的 turn_id == tids[0], tids[1]。
        if let RolloutRecord::Message { turn_id, .. } = &records[1] {
            assert_eq!(*turn_id, tids[0]);
        } else {
            panic!("records[1] 应是 Message");
        }
        if let RolloutRecord::Message { turn_id, .. } = &records[2] {
            assert_eq!(*turn_id, tids[1]);
        } else {
            panic!("records[2] 应是 Message");
        }
    }

    #[tokio::test]
    async fn truncate_after_none_drops_last_turn() {
        let dir = tempdir().unwrap();
        let (w, sid, _tids) = seed_three_turns(dir.path()).await;
        // None = 丢弃最后一个 turn(tids[4] + tids[5] 属同一 turn? 这里每条
        // Message 有独立 turn_id,故 None 丢弃最后一个 turn_id 的整 turn
        // = tids[5])。
        let dropped = w.truncate_after(None).await.unwrap();
        assert_eq!(dropped, 1, "None 应丢弃最后一条 Message");

        let records = w.replay(sid).await.unwrap();
        // SessionMeta + 5 条 Message。
        assert_eq!(records.len(), 6, "got: {records:?}");
    }

    #[tokio::test]
    async fn truncate_after_unknown_turn_is_noop() {
        let dir = tempdir().unwrap();
        let (w, sid, _tids) = seed_three_turns(dir.path()).await;
        let before = w.replay(sid).await.unwrap();
        let dropped = w
            .truncate_after(Some(&TurnId::new())) // 不存在的 tid
            .await
            .unwrap();
        assert_eq!(dropped, 0, "未知 tid 应是 no-op");
        let after = w.replay(sid).await.unwrap();
        assert_eq!(before.len(), after.len());
        // 不应产生 .bak 备份(未改动)。
        assert!(
            walkdir(dir.path()).iter().all(|p| !p.contains(".bak")),
            "no-op 不应写备份"
        );
    }

    #[tokio::test]
    async fn truncate_after_keeps_session_meta_first_line() {
        let dir = tempdir().unwrap();
        let (w, sid, tids) = seed_three_turns(dir.path()).await;
        // 截断到第一个 Message(tids[0])→ 只剩 SessionMeta。
        let dropped = w.truncate_after(Some(&tids[0])).await.unwrap();
        assert_eq!(dropped, 6, "丢弃全部 6 条 Message");

        let records = w.replay(sid).await.unwrap();
        assert_eq!(records.len(), 1, "只剩 SessionMeta");
        assert!(matches!(records[0], RolloutRecord::SessionMeta { .. }));
    }

    #[tokio::test]
    async fn truncate_after_creates_backup_file() {
        let dir = tempdir().unwrap();
        let (w, sid, tids) = seed_three_turns(dir.path()).await;
        // 先记下截断前的全文。
        let path = session_path_at(dir.path(), sid, chrono::Utc::now());
        let pre_content = std::fs::read_to_string(&path).unwrap();

        let _ = w.truncate_after(Some(&tids[3])).await.unwrap();

        // 应存在一个 .bak 文件,内容 == 截断前全文。
        let mut bak_content: Option<String> = None;
        for p in walkdir(dir.path()) {
            if p.contains(".rewind-") && p.ends_with(".bak") {
                bak_content = Some(std::fs::read_to_string(&p).unwrap());
                break;
            }
        }
        let bak = bak_content.expect("应存在 .bak 备份");
        assert_eq!(bak, pre_content, "备份应 == 截断前全文");
    }

    #[tokio::test]
    async fn truncate_after_then_record_appends_correctly() {
        let dir = tempdir().unwrap();
        let (w, sid, tids) = seed_three_turns(dir.path()).await;
        // 截断到中间,再追加一条新 Message,确认文件可继续写
        // + SessionMeta 仍在第一行。
        let _ = w.truncate_after(Some(&tids[2])).await.unwrap(); // 剩 SessionMeta + 2 Message
        let new_tid = TurnId::new();
        w.record(RolloutRecord::message(
            new_tid,
            MessageRole::User,
            serde_json::json!("post-rewind"),
        ))
        .await
        .unwrap();

        let records = w.replay(sid).await.unwrap();
        // SessionMeta + 2(保留) + 1(新)= 4。
        assert_eq!(records.len(), 4, "got: {records:?}");
        assert!(matches!(records[0], RolloutRecord::SessionMeta { .. }));
        // 最后一条是新 turn_id。
        if let RolloutRecord::Message { turn_id, .. } = records.last().unwrap() {
            assert_eq!(*turn_id, new_tid);
        } else {
            panic!("最后一条应是新 Message");
        }
    }
}
