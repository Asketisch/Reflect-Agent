//! `reflect-notes` — 会话级增量笔记系统(v1.1.0 Phase 6 P0)。
//!
//! 提供三类能力:
//!
//! 1. [`NoteStore`] trait + [`InMemoryNoteStore`] — FIFO 30 条的纯内存队列,
//!    与 `reflect-memory` 的 `MEMORY.md` 大块记忆互补。
//! 2. [`FileBackedNoteStore`] — 在 `InMemoryNoteStore` 之上叠加 append-only
//!    JSONL 落盘,重启时自动 rehydrate 最近 30 条,跨进程保留关键发现。
//! 3. [`AddSessionNoteTool`] — `add_session_note` 工具,实现
//!    [`reflect_tools::Tool`],LLM 在每轮中可主动追加笔记。
//!
//! ## 路径解析
//!
//! `resolve_notes_home()` 优先读 `REFLECT_HOME` 环境变量,否则 fallback
//! 到 `$HOME/.reflect`。JSONL 文件命名 `<home>/session-notes/<thread_id>.jsonl`,
//! 每行一条 `{"text": ..., "created_at": ...}`,append-only + `sync_data()`
//! 持久化。
//!
//! ## 设计差异
//!
//! 本 crate 取**零成本 LLM-free 路径**,但额外加
//! JSONL 落盘,平衡"零 LLM 成本"和"重启可恢复"两个目标。

#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_borrows_for_generic_args)]

pub mod error;
pub mod persisted;
pub mod store;
pub mod tool;

pub use error::NoteError;
pub use persisted::FileBackedNoteStore;
pub use store::{InMemoryNoteStore, NoteStore, SESSION_NOTE_CAP, SessionNote};
pub use tool::AddSessionNoteTool;

/// 解析 JSONL notes 落盘目录。
///
/// 优先级:`$REFLECT_HOME` > `$HOME/.reflect`。`REFLECT_HOME` 未设置且
/// `HOME` 未设置时返回 `None` —— caller 应 fallback 到纯 RAM store。
pub fn resolve_notes_home() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("REFLECT_HOME") {
        return Some(std::path::PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".reflect"))
}
