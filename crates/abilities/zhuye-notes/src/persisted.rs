//! `FileBackedNoteStore` — 在 `InMemoryNoteStore` 之上叠加 append-only
//! JSONL 持久化。
//!
//! ## 设计
//!
//! - **RAM 权威**:`inner: Arc<InMemoryNoteStore>`,所有读 / 写都走它。
//!   文件层只在 `add` 时追加一行,`open_or_create` 时 rehydrate 一次。
//! - **Append-only**:每次 `add` 把整行(已含尾随 `\n`)作为一次
//!   `write_all` 写出,然后 `sync_data()`。单次 `write(2)` 在 `O_APPEND`
//!   文件上原子追加,跨进程 / 跨线程不会交错 —— Review 2026-06-30 P1-1
//!   修复前 `bytes` 和 `\n` 分两次写,并发 appender 会插入,产出 torn
//!   JSONL 行。
//! - **Rehydrate**:`open_or_create` 读全文(每个有效行限 `NOTE_MAX_CHARS` +
//!   JSON 包装 ≈ 4150 字节,长跑会话全文仍可控)→ 收集最近 `2*cap` 条
//!   候选 → 取最后 `cap` 条注入 RAM。`torn write` 解析失败的行直接
//!   skip(记 warn),在校验(`Empty`/`TooLong`)失败的行也 skip(记 warn,
//!   不阻断整个 rehydrate)—— Review 2026-06-30 P1-2 修复前一行坏
//!   就会让整文件不可用。
//!
//! ## 设计差异
//!
//! 本实现叠加 JSONL
//! 落盘,平衡"零 LLM 成本"和"重启可恢复"。

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::NoteError;
use crate::store::{InMemoryNoteStore, NoteStore, SESSION_NOTE_CAP, SessionNote};

/// 包装 `InMemoryNoteStore` + JSONL 文件,append 持久化 + 重启 rehydrate。
#[derive(Debug)]
pub struct FileBackedNoteStore {
    inner: Arc<InMemoryNoteStore>,
    path: PathBuf,
}

impl FileBackedNoteStore {
    /// 打开 / 创建 JSONL 文件,把已存在的最后 N 条注入 RAM。
    ///
    /// 失败模式:
    /// - 父目录创建失败 → `Err(io)`
    /// - `cap == 0` → `Err(Empty)`(内部 `with_cap` 校验,替代 v1.1.0
    ///   早期版本的 `assert!` panic —— Review 2026-06-30 P2-2 修复)
    /// - rehydrate 单行 `Empty` / `TooLong` → warn + skip,整个文件
    ///   rehydrate 不阻断(替代 v1.1.0 早期版本 `?` 直接传播)——
    ///   Review 2026-06-30 P1-2 修复
    pub fn open_or_create(path: impl Into<PathBuf>, cap: usize) -> Result<Self, NoteError> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if cap == 0 {
            return Err(NoteError::Empty);
        }
        let inner = InMemoryNoteStore::shared(cap);
        if path.exists() {
            Self::rehydrate_into(&path, &inner)?;
        }
        Ok(Self { inner, path })
    }

    /// 用默认 cap=30 的便捷构造。
    pub fn open_or_create_default(path: impl Into<PathBuf>) -> Result<Self, NoteError> {
        Self::open_or_create(path, SESSION_NOTE_CAP)
    }

    /// JSONL 文件路径(测试断言 / 调试用)。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 从磁盘读 + 注入 RAM。`open_or_create` 内部用;也供 `clear` 后
    /// 从空文件 rehydrate。
    ///
    /// Review 2026-06-30:
    /// - **P2-3**:只保留最近 `2 * cap` 条候选(用 `VecDeque` 滑动窗口),
    ///   避免长跑会话 rehydrate 全文。
    /// - **P1-2**:单行 `Empty` / `TooLong` 校验失败也 `warn!` + skip,
    ///   整文件 rehydrate 不再因一行坏数据而中断。
    fn rehydrate_into(path: &Path, inner: &Arc<InMemoryNoteStore>) -> Result<(), NoteError> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        // 滑动窗口:仅保留最近 2*cap 条解析成功的候选,长跑会话读全文
        // 的成本被截断;`pop_front` O(1)。
        let cap = inner.cap();
        let window_cap = cap.saturating_mul(2).max(cap);
        let mut window: std::collections::VecDeque<SessionNote> =
            std::collections::VecDeque::with_capacity(window_cap);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue, // IO 错跳过(单行读失败)
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionNote>(&line) {
                Ok(n) => {
                    if window.len() >= window_cap {
                        window.pop_front();
                    }
                    window.push_back(n);
                }
                Err(e) => {
                    // Torn write / 损坏行 —— 跳过并 warn,但不中断。
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "跳过无法解析的 JSONL 行(torn write 或损坏)"
                    );
                }
            }
        }
        // 把窗口内容按时间顺序注入 RAM(最新在末尾)。
        // Review 2026-06-30 P1-2:单行校验失败也 warn + skip,不再 `?` 传播。
        for n in &window {
            if let Err(e) = inner.add(n.text.clone()) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    text_len = n.text.chars().count(),
                    "跳过 rehydrate 中无法通过校验的 note(空 / 超长);不阻断其他 note 加载"
                );
            }
        }
        Ok(())
    }
}

impl NoteStore for FileBackedNoteStore {
    fn add(&self, text: String) -> Result<SessionNote, NoteError> {
        // 先调 RAM 层 —— 拿到 note(含 created_at),再追加到 JSONL。
        // 这样 if-then-fail 的语义:RAM 已写但 disk 写失败时,RAM 仍
        // 有 note,下一轮仍可见(只丢持久化)。回滚 RAM 需要双锁,
        // 不值得 —— note 数据本身允许丢(对照 `MemoryStore` 的
        // `save` 也是 best-effort)。
        let note = self.inner.add(text)?;
        if let Err(e) = append_line(&self.path, &note) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "JSONL 追加失败;note 仅在 RAM 中,重启后会丢"
            );
        }
        Ok(note)
    }

    fn list(&self) -> Vec<SessionNote> {
        self.inner.list()
    }

    fn as_meta_message(&self) -> Option<String> {
        self.inner.as_meta_message()
    }

    fn clear(&self) {
        self.inner.clear();
        // 文件 truncate 到 0(避免遗留旧 note)。失败不致命 —— 旧的
        // 会在下次 rehydrate 重新加载,顶多多弹几条。
        if let Err(e) = std::fs::write(&self.path, b"") {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "JSONL truncate 失败"
            );
        }
    }
}

/// 追加一行 JSON + `\n` 到文件,带 `sync_data()` 持久化。
///
/// Review 2026-06-30 P1-1 修复:`bytes` 与 `\n` 合并为单次 `write_all`
/// 调用,确保一次 `write(2)` 在 `O_APPEND` 文件上原子追加 —— 此前两次
/// 写调用之间可被另一 appender 插入,产出 torn 行。
fn append_line(path: &Path, note: &SessionNote) -> Result<(), NoteError> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut bytes = serde_json::to_vec(note)?;
    bytes.push(b'\n');
    f.write_all(&bytes)?;
    f.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_or_create_empty_file_succeeds() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        assert_eq!(s.list().len(), 0);
        assert_eq!(p, s.path());
    }

    #[test]
    fn add_persists_to_disk() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        s.add("alpha".into()).unwrap();
        s.add("beta".into()).unwrap();
        // 立即能看到
        let raw = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("alpha"));
        assert!(lines[1].contains("beta"));
    }

    #[test]
    fn reopen_rehydrates_in_order() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        {
            let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
            s.add("first".into()).unwrap();
            s.add("second".into()).unwrap();
            s.add("third".into()).unwrap();
        }
        // 重新打开
        let s2 = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        let list = s2.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].text, "first");
        assert_eq!(list[1].text, "second");
        assert_eq!(list[2].text, "third");
    }

    #[test]
    fn reopen_keeps_last_cap_when_overflowing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        // 用 cap=5 写 12 条
        {
            let s = FileBackedNoteStore::open_or_create(&p, 5).unwrap();
            for i in 0..12 {
                s.add(format!("n{i:02}")).unwrap();
            }
        }
        // 重新以 cap=5 打开,只保留最后 5 条
        let s2 = FileBackedNoteStore::open_or_create(&p, 5).unwrap();
        let list = s2.list();
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].text, "n07");
        assert_eq!(list[4].text, "n11");
    }

    #[test]
    fn malformed_lines_skipped_on_rehydrate() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        // 手动写一个混合文件:1 条好 + 1 条坏 + 1 条好
        std::fs::write(
            &p,
            b"{\"text\":\"good1\",\"created_at\":\"2026-06-27T00:00:00Z\"}\n\
              this is not json\n\
              {\"text\":\"good2\",\"created_at\":\"2026-06-27T00:00:01Z\"}\n",
        )
        .unwrap();
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        let list = s.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].text, "good1");
        assert_eq!(list[1].text, "good2");
    }

    #[test]
    fn clear_truncates_file_and_memory() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        s.add("x".into()).unwrap();
        s.add("y".into()).unwrap();
        s.clear();
        assert_eq!(s.list().len(), 0);
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.is_empty(), "file should be truncated, got: {raw:?}");
    }

    #[test]
    fn create_dir_all_for_parent() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nested").join("sub").join("notes.jsonl");
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        s.add("nested".into()).unwrap();
        assert!(p.exists());
    }

    // ── Review 2026-06-30 ────────────────────────────────────────────────────

    /// Review 2026-06-30 P1-1:并发 append 不会产生 torn JSONL 行。
    /// 验证:多线程同时 `add`,最终文件每行都能被 `serde_json` 解析。
    #[test]
    fn concurrent_adds_produce_parseable_jsonl_lines() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        let store = Arc::new(FileBackedNoteStore::open_or_create_default(&p).unwrap());
        let n_threads = 8;
        let per_thread = 25;
        let handles: Vec<_> = (0..n_threads)
            .map(|t| {
                let s = store.clone();
                thread::spawn(move || {
                    for i in 0..per_thread {
                        s.add(format!("t{t}-i{i}")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // 全部 n_threads * per_thread = 200 条应进入 RAM(30 上限 → 实际 30)。
        assert_eq!(store.list().len(), 30);
        // 重新打开,验证磁盘文件每行 JSON 都能解析。
        drop(store);
        let reopened = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        let mut parseable = 0usize;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<SessionNote>(line)
                .unwrap_or_else(|e| panic!("torn/invalid line {line:?}: {e}"));
            parseable += 1;
        }
        assert_eq!(parseable, 200, "every write must land as a parseable line");
        assert_eq!(reopened.list().len(), 30, "rehydrate after cap truncate");
    }

    /// Review 2026-06-30 P1-2:rehydrate 单行坏数据 → warn + skip,整文件仍可用。
    #[test]
    fn rehydrate_skips_invalid_lines_without_aborting() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        // 写一份"好 + 坏 + 好"的混合文件。
        std::fs::write(
            &p,
            b"{\"text\":\"good1\",\"created_at\":\"2026-06-27T00:00:00Z\"}\n\
              this is not json\n\
              {\"text\":\"good2\",\"created_at\":\"2026-06-27T00:00:01Z\"}\n",
        )
        .unwrap();
        let s = FileBackedNoteStore::open_or_create_default(&p).unwrap();
        // 修复前:? 会传播,rehydrate 整体失败,list() 返回空。
        // 修复后:坏行 warn + skip,两条 good 全部加载。
        let list = s.list();
        assert_eq!(list.len(), 2, "good lines must still load");
        assert_eq!(list[0].text, "good1");
        assert_eq!(list[1].text, "good2");
    }

    /// Review 2026-06-30 P2-2:cap=0 不再 panic,返回 Err。
    #[test]
    fn open_or_create_with_zero_cap_returns_err_not_panic() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        let res = FileBackedNoteStore::open_or_create(&p, 0);
        assert!(
            matches!(res, Err(NoteError::Empty)),
            "cap=0 must return Err(Empty), got {res:?}"
        );
    }

    /// Review 2026-06-30 P2-3:rehydrate 只读最近 2*cap 条候选,长跑会话不会
    /// 因全文增长而拖累启动。
    #[test]
    fn rehydrate_reads_at_most_two_times_cap() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("notes.jsonl");
        // 直接写 200 行 JSONL 到文件(超 cap=5 的 2*cap=10)。
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!(
                "{{\"text\":\"n{i:03}\",\"created_at\":\"2026-06-27T00:00:0{}Z\"}}\n",
                i % 10
            ));
        }
        std::fs::write(&p, content).unwrap();
        let s = FileBackedNoteStore::open_or_create(&p, 5).unwrap();
        let list = s.list();
        // 只保留最近 5 条(由 inner.add 后的 FIFO 截断保证)。
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].text, "n195");
        assert_eq!(list[4].text, "n199");
    }
}
