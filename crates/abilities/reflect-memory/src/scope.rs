//! memory scope 的路径解析。
//!
//! 镜像 Reflect `agent_memory.py:resolve_memory_path`。
//!
//! - `Project` → `{workspace}/.reflect/agent-memory/{agent_type}/MEMORY.md`
//! - `User`    → `{HOME}/.reflect/agent-memory/{agent_type}/MEMORY.md`
//! - `Session` → 无文件承载;由 [`crate::store::InMemoryStore`] 处理。

use std::path::{Path, PathBuf};

use crate::model::{MemoryError, MemoryScope};

/// 从 memory 注入 system prompt 的字符硬上限。
/// 镜像 Reflect `_MAX_MEMORY_INJECT_CHARS`。
pub const MAX_MEMORY_INJECT_CHARS: usize = 8000;

/// 将 `agent_type` 净化为可用目录名。镜像 Reflect:
/// 把 `:` `/` `\\` 替换为 `-`。
///
/// review 修复:`..` 不含分隔符,此前原样通过 —— `agent_type=".."` 会把
/// MEMORY.md 写到 `agent-memory/..` 之外(`.reflect/MEMORY.md`),LLM 可经
/// memory 工具触发。补充把 `.` 段与控制字符折叠为 `-`。
pub fn sanitize_agent_type(agent_type: &str) -> String {
    let cleaned: String = agent_type
        .chars()
        .map(|c| match c {
            ':' | '/' | '\\' => '-',
            c if c.is_control() => '-',
            _ => c,
        })
        .collect();
    // 清洗后仍为 `.` / `..`(纯点段)一律折叠为 `-`,杜绝目录穿越。
    if cleaned == "." || cleaned == ".." {
        "-".to_string()
    } else {
        cleaned
    }
}

/// 计算 `(scope, agent_type)` 对应的磁盘路径。`Session` 返回错误
/// (它没有文件)。
///
/// `home` 缺省取 `std::env::var("HOME")`(或平台用户主目录);
/// 测试可显式传入。
pub fn resolve_path(
    scope: MemoryScope,
    workspace: &Path,
    home: &Path,
    agent_type: &str,
) -> Result<PathBuf, MemoryError> {
    if agent_type.is_empty() {
        return Err(MemoryError::Invalid("agent_type is empty".into()));
    }
    let safe = sanitize_agent_type(agent_type);
    match scope {
        MemoryScope::Project => Ok(workspace
            .join(".reflect")
            .join("agent-memory")
            .join(safe)
            .join("MEMORY.md")),
        MemoryScope::User => Ok(home
            .join(".reflect")
            .join("agent-memory")
            .join(safe)
            .join("MEMORY.md")),
        MemoryScope::Session => Err(MemoryError::SessionNotPersisted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_separators() {
        assert_eq!(sanitize_agent_type("foo:bar/baz\\qux"), "foo-bar-baz-qux");
        assert_eq!(sanitize_agent_type("plain"), "plain");
        assert_eq!(sanitize_agent_type(""), "");
    }

    #[test]
    fn project_path_is_workspace_relative() {
        let p = resolve_path(
            MemoryScope::Project,
            Path::new("/work"),
            Path::new("/home"),
            "default",
        )
        .unwrap();
        assert_eq!(
            p,
            PathBuf::from("/work/.reflect/agent-memory/default/MEMORY.md")
        );
    }

    #[test]
    fn user_path_is_home_relative() {
        let p = resolve_path(
            MemoryScope::User,
            Path::new("/work"),
            Path::new("/home/u"),
            "default",
        )
        .unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/u/.reflect/agent-memory/default/MEMORY.md")
        );
    }

    #[test]
    fn session_path_errors() {
        let err = resolve_path(
            MemoryScope::Session,
            Path::new("/work"),
            Path::new("/home"),
            "default",
        )
        .unwrap_err();
        assert!(matches!(err, MemoryError::SessionNotPersisted));
    }

    #[test]
    fn empty_agent_type_errors() {
        let err = resolve_path(
            MemoryScope::Project,
            Path::new("/work"),
            Path::new("/home"),
            "",
        )
        .unwrap_err();
        assert!(matches!(err, MemoryError::Invalid(_)));
    }

    #[test]
    fn unsafe_agent_type_is_sanitized() {
        let p = resolve_path(
            MemoryScope::Project,
            Path::new("/work"),
            Path::new("/home"),
            "evil:agent/name",
        )
        .unwrap();
        assert_eq!(
            p,
            PathBuf::from("/work/.reflect/agent-memory/evil-agent-name/MEMORY.md")
        );
    }
}
