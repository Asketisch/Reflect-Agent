//! `MemoryScope` 与错误类型。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 三种 memory scope。
///
/// - `Project` —— 相对 workspace,可入 VCS 共享。
/// - `User` —— 相对 home,跨项目共享。
/// - `Session` —— 仅内存;`FileMemoryStore` 从不持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Project,
    User,
    Session,
}

impl std::fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MemoryScope::Project => "project",
            MemoryScope::User => "user",
            MemoryScope::Session => "session",
        })
    }
}

/// memory 相关错误。
#[derive(Debug, Error)]
pub enum MemoryError {
    /// I/O 错误(文件不存在、权限拒绝等)。
    #[error("memory io error: {0}")]
    Io(#[from] std::io::Error),
    /// `Session` scope 无法持久化到磁盘。
    #[error("session memory cannot be persisted to disk")]
    SessionNotPersisted,
    /// scope / agent_type 组合非法。
    #[error("invalid memory configuration: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_display_matches_serde() {
        assert_eq!(MemoryScope::Project.to_string(), "project");
        assert_eq!(MemoryScope::User.to_string(), "user");
        assert_eq!(MemoryScope::Session.to_string(), "session");
    }

    #[test]
    fn scope_serde_roundtrip() {
        for s in [
            MemoryScope::Project,
            MemoryScope::User,
            MemoryScope::Session,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: MemoryScope = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }
}
