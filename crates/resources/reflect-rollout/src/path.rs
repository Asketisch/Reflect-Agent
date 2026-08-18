//! Rollout JSONL 文件的磁盘布局。
//!
//! 布局:`<base>/YYYY/MM/DD/<thread_id>.jsonl`,其中日期为打开 session
//! 那一刻的 UTC 日期。轮转文件是
//! `<base>/YYYY/MM/DD/<thread_id>.<n>.jsonl`,`n in 1..=MAX_ROTATED_FILES`。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::types::DEFAULT_ROLLOUT_DIR;
use reflect_protocol::ThreadId;

/// 带显式时间戳的 session 路径变体 —— 供测试与回放历史 session
/// 的 caller 使用。
pub fn session_path_at(base: &Path, session_id: ThreadId, at: DateTime<Utc>) -> PathBuf {
    base.join(format!("{:04}", at.format("%Y").to_string()))
        .join(format!("{:02}", at.format("%m").to_string()))
        .join(format!("{:02}", at.format("%d").to_string()))
        .join(format!("{}.jsonl", session_id))
}

/// 默认 base 目录:`$HOME/.reflect/sessions`。
pub fn default_base() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(DEFAULT_ROLLOUT_DIR),
        None => PathBuf::from(DEFAULT_ROLLOUT_DIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_segmentation() {
        let sid = ThreadId::new();
        let base = Path::new("/tmp/reflect");
        let at: DateTime<Utc> = "2026-06-18T12:00:00Z".parse().unwrap();
        let p = session_path_at(base, sid, at);
        let expected = base
            .join("2026")
            .join("06")
            .join("18")
            .join(format!("{}.jsonl", sid));
        assert_eq!(p, expected);
    }

    #[test]
    fn default_base_uses_home_env() {
        let p = default_base();
        assert!(p.ends_with(DEFAULT_ROLLOUT_DIR), "got {p:?}");
    }
}
