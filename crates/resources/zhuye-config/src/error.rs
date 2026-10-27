//! 统一配置错误类型。

use thiserror::Error;

/// Phase 4 收尾:加 `#[non_exhaustive]` —— 后续新增 variant 不破坏 caller
/// 的 `?` 传播 / `Debug` 输出。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("watch: {0}")]
    Watch(String),
    #[error("build: {0}")]
    Build(String),
    /// Phase 4:`load_from_str_with_migration` 走 sequential migration
    /// 时遇到未知起始版本号 / 链断。区分 builder 失败(provider construction)
    /// vs migration 失败(schema 版本升级)。
    #[error("migration: {0}")]
    Migration(String),
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e.to_string())
    }
}
