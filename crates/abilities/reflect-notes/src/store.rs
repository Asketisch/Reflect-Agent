//! `SessionNote` / `NoteStore` trait / `InMemoryNoteStore`。
//!
//! 镜像 `reflect_memory::store.rs` 的 trait + 双实现 pattern:把
//! `MemoryStore` 的"大块字符串"模型换成"append-only 列表"模型,
//! 但保持同样的"trait + RAM + 文件包装"分层。

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::NoteError;

/// 单条 note 的硬上限(超过返回 `NoteError::TooLong`)。
pub const NOTE_MAX_CHARS: usize = 4096;

/// 笔记队列的默认容量。
pub const SESSION_NOTE_CAP: usize = 30;

/// `as_meta_message()` 渲染时的字节预算。
///
/// 每轮 `<system-reminder>` 注入会被模型重读,单 turn 文本过大(超过
/// Anthropic / OpenAI 的 prompt cache 4 KiB 窗口)会让 cache miss 累积。
/// 8 KiB 与 `reflect_memory::truncate_for_injection` 的 memory 截断
/// 阈值对齐 —— 假设 session notes 与 memory 是用户上下文的两条主
/// 注入源,它们应保持同量级。Review 2026-06-30 P2-1 修复:此前无
/// 字节预算,30 × 4096 字符 ≈ 123 KiB 会撑爆 prompt cache。
pub const META_MESSAGE_BUDGET_BYTES: usize = 8 * 1024;

/// 单条 session note。
///
/// `text` 是 LLM 主动追加的关键发现 / 用户偏好 / 中间结论;
/// `created_at` 用于按时间排序 + 调试追溯。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNote {
    pub text: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl SessionNote {
    /// 构造并 stamp `created_at = now`。
    pub fn new_now(text: String) -> Self {
        Self {
            text,
            created_at: chrono::Utc::now(),
        }
    }
}

/// 笔记存储抽象。`Send + Sync` 是为了塞进 `M4Deps` 后跨 turn 共享。
///
/// 实现分两层:`InMemoryNoteStore` 是 RAM 权威,`FileBackedNoteStore`
/// 在它之上叠加 append-only JSONL 落盘 + 重启 rehydrate。
pub trait NoteStore: Send + Sync + std::fmt::Debug {
    /// 追加一条 note。FIFO 满则弹出最早一条。空文本 / 超长返回错误。
    fn add(&self, text: String) -> Result<SessionNote, NoteError>;

    /// 当前所有 note(按插入顺序,最新在末尾)。
    fn list(&self) -> Vec<SessionNote>;

    /// 渲染为 `<system-reminder>` 注入内容。空时返回 `None` —— caller
    /// 据此跳过 meta-message 注入(避免浪费 token)。
    fn as_meta_message(&self) -> Option<String>;

    /// 清空(测试 / 强制重置用)。文件层 wrapper 实现同时 truncate JSONL。
    fn clear(&self);
}

/// 纯 RAM 的 FIFO 30 笔记存储。
///
/// `Mutex<VecDeque>` 单一锁即可:`add` 截断 + push + 超 cap pop front
/// 是 O(1);`as_meta_message` 只在 `pre_loop` 调,无热点压力。
/// `Arc<InMemoryNoteStore>` 模式让 `FileBackedNoteStore` 可以共享
/// RAM 权威。
pub struct InMemoryNoteStore {
    inner: Mutex<VecDeque<SessionNote>>,
    cap: usize,
}

impl std::fmt::Debug for InMemoryNoteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryNoteStore")
            .field("count", &self.inner.lock().len())
            .field("cap", &self.cap)
            .finish()
    }
}

impl Default for InMemoryNoteStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryNoteStore {
    /// 默认 cap=30 的空 store。
    pub fn new() -> Self {
        Self::with_cap(SESSION_NOTE_CAP)
    }

    /// 自定义 cap(测试或特殊场景用)。
    pub fn with_cap(cap: usize) -> Self {
        assert!(cap > 0, "InMemoryNoteStore cap must be > 0");
        Self {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    /// 共享 RAM 层给 `FileBackedNoteStore` 包装。`Arc` 让两层共享
    /// 同一份 `Mutex<VecDeque>`,append / rehydrate 不会复制。
    pub fn shared(cap: usize) -> Arc<Self> {
        Arc::new(Self::with_cap(cap))
    }

    /// 当前 cap(FIFO 上限)。`FileBackedNoteStore::rehydrate_into` 用它
    /// 计算"取最后 N 条"。
    pub fn cap(&self) -> usize {
        self.cap
    }
}

impl NoteStore for InMemoryNoteStore {
    fn add(&self, text: String) -> Result<SessionNote, NoteError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(NoteError::Empty);
        }
        if trimmed.chars().count() > NOTE_MAX_CHARS {
            return Err(NoteError::TooLong(trimmed.chars().count()));
        }
        let note = SessionNote::new_now(trimmed.to_string());
        let mut guard = self.inner.lock();
        while guard.len() >= self.cap {
            guard.pop_front();
        }
        guard.push_back(note.clone());
        Ok(note)
    }

    fn list(&self) -> Vec<SessionNote> {
        self.inner.lock().iter().cloned().collect()
    }

    fn as_meta_message(&self) -> Option<String> {
        let notes = self.inner.lock();
        if notes.is_empty() {
            return None;
        }
        // Review 2026-06-30 P2-1 修复:`META_MESSAGE_BUDGET_BYTES` 字节预算。
        // review 修复:此前按插入顺序从前往后写、超预算 break —— 丢的是
        // **最新**的笔记,与设计意图(新笔记更相关,淘汰旧笔记)相反。
        // 现从最新端往前收集,装得下的连续最新段按时间序输出;
        // 被截断的是最旧的 note(与 FIFO 淘汰语义一致)。
        let header = "[Session Memory — 本次会话关键笔记]\n";
        let truncation_marker = "… (older notes truncated to fit budget)\n";
        let marker_len = truncation_marker.len();
        // 从最新端往前累计字节,找到能放进预算的连续后缀。
        // 截断说明的预算始终预留(保守):仅当确实有旧 note 被截掉时追加。
        let mut start_idx = notes.len();
        let mut bytes_needed = header.len() + marker_len;
        for (i, n) in notes.iter().enumerate().rev() {
            let line_bytes = format!("{}. {}\n", i + 1, n.text).len();
            let projected = bytes_needed + line_bytes;
            if projected > META_MESSAGE_BUDGET_BYTES {
                break;
            }
            bytes_needed += line_bytes;
            start_idx = i;
        }
        let mut s = String::from(header);
        for (i, n) in notes.iter().enumerate().skip(start_idx) {
            s.push_str(&format!("{}. {}\n", i + 1, n.text));
        }
        if start_idx > 0 {
            s.push_str(truncation_marker);
        }
        Some(s)
    }

    fn clear(&self) {
        self.inner.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_fifo_evicts_oldest_at_cap() {
        let s = InMemoryNoteStore::with_cap(3);
        for i in 0..5 {
            s.add(format!("n{i}")).unwrap();
        }
        let list = s.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].text, "n2");
        assert_eq!(list[2].text, "n4");
    }

    #[test]
    fn in_memory_rejects_empty_text() {
        let s = InMemoryNoteStore::new();
        assert!(matches!(s.add("".into()), Err(NoteError::Empty)));
        assert!(matches!(s.add("   \n\t  ".into()), Err(NoteError::Empty)));
    }

    #[test]
    fn in_memory_rejects_overlong_text() {
        let s = InMemoryNoteStore::new();
        let big = "x".repeat(NOTE_MAX_CHARS + 1);
        assert!(matches!(s.add(big), Err(NoteError::TooLong(_))));
    }

    #[test]
    fn in_memory_meta_message_none_when_empty() {
        let s = InMemoryNoteStore::new();
        assert!(s.as_meta_message().is_none());
    }

    #[test]
    fn in_memory_meta_message_includes_numbered_list() {
        let s = InMemoryNoteStore::new();
        s.add("first".into()).unwrap();
        s.add("second".into()).unwrap();
        let m = s.as_meta_message().unwrap();
        assert!(m.contains("1. first"));
        assert!(m.contains("2. second"));
    }

    #[test]
    fn in_memory_clear_empties_list() {
        let s = InMemoryNoteStore::new();
        s.add("x".into()).unwrap();
        s.clear();
        assert_eq!(s.list().len(), 0);
        assert!(s.as_meta_message().is_none());
    }

    #[test]
    fn trim_is_applied_before_validation() {
        let s = InMemoryNoteStore::new();
        let n = s.add("  hello  ".into()).unwrap();
        assert_eq!(n.text, "hello");
    }

    #[test]
    fn shared_returns_arc_with_correct_cap() {
        let s = InMemoryNoteStore::shared(2);
        s.add("a".into()).unwrap();
        s.add("b".into()).unwrap();
        s.add("c".into()).unwrap();
        assert_eq!(s.list().len(), 2);
        assert_eq!(s.list()[0].text, "b");
    }

    // ── Review 2026-06-30 ────────────────────────────────────────────────────

    /// Review 2026-06-30 P2-1:`as_meta_message` 在字节预算内截断,
    /// 且截断说明被附上(只要预算还放得下)。
    ///
    /// review 修复后的语义:保留的是**最新**的连续后缀(新笔记更相关),
    /// 被截断的是最旧的 note —— 此前实现反了(丢最新),测试随之更新。
    #[test]
    fn meta_message_respects_byte_budget_and_annotates_truncation() {
        let s = InMemoryNoteStore::new();
        // 用 ~600 字符的 note 塞 30 条:30 × 610 ≈ 18 KiB 远超 8 KiB
        // 预算,只能保留最新的一段。索引越大越新,被截的是最旧的 0..=k。
        let big = "x".repeat(600);
        for i in 0..30 {
            s.add(format!("{big}-{i:02}")).unwrap();
        }
        let msg = s.as_meta_message().expect("non-empty");
        assert!(
            msg.len() <= META_MESSAGE_BUDGET_BYTES,
            "meta-message len {} exceeds budget {}",
            msg.len(),
            META_MESSAGE_BUDGET_BYTES
        );
        assert!(
            msg.contains("truncated to fit budget"),
            "truncation marker must be present, got head: {:?}",
            &msg[..msg.len().min(200)]
        );
        // 最新的(-29)必须保留;最旧的(-00)应被截掉。
        assert!(msg.contains("-29"), "newest must be retained: {msg}");
        assert!(!msg.contains("-00"), "oldest truncated: {msg}");
    }

    /// Review 2026-06-30 P2-1:预算极紧张时(只剩截断说明长度),跳过 marker。
    #[test]
    fn meta_message_drops_marker_when_budget_too_tight() {
        let s = InMemoryNoteStore::with_cap(2);
        // 两条 note 总长度接近预算,预留 marker 后无空间。
        let big = "y".repeat(META_MESSAGE_BUDGET_BYTES / 2 - 16);
        s.add(big.clone()).unwrap();
        s.add(big).unwrap();
        let msg = s.as_meta_message().expect("non-empty");
        // 仍然 ≤ 预算(预算紧时只发已有行,marker 可省)。
        assert!(msg.len() <= META_MESSAGE_BUDGET_BYTES);
    }

    /// Review 2026-06-30 P2-1:空 store 仍然返回 None(无回归)。
    #[test]
    fn meta_message_none_when_empty_after_fix() {
        let s = InMemoryNoteStore::new();
        assert!(s.as_meta_message().is_none());
    }
}
