//! `NoteError` — 笔记存取失败时的统一错误类型。

use thiserror::Error;

/// 笔记存储相关错误。
#[derive(Debug, Error)]
pub enum NoteError {
    /// 笔记文本超过单条上限(4096 字符,trim 后)。
    #[error("note text too long: {0} chars (max 4096 after trim)")]
    TooLong(usize),

    /// 笔记文本为空(trim 后)。
    #[error("note text is empty after trim")]
    Empty,

    /// I/O 错误(append / rehydrate 时读盘 / 写盘失败)。
    #[error("note io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 反序列化失败(torn write / 损坏行被跳过,这条表示整文件
    /// 不可恢复)。
    #[error("note deserialize error: {0}")]
    Serde(#[from] serde_json::Error),
}
