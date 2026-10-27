//! `Hook` trait + `HookError`。

use async_trait::async_trait;
use thiserror::Error;

use crate::decision::HookDecision;
use crate::event::{HookEvent, HookEventKind};

/// agent 循环中用户可扩展的拦截点。
#[async_trait]
pub trait Hook: Send + Sync {
    /// 稳定标识(用于 tracing 与 `HookEngine` 调试输出)。
    fn name(&self) -> &str;

    /// 单行人类可读描述(供 `/hooks ls` TUI pill、插件 manifest、
    /// `/doctor` 诊断输出使用)。默认返回空串 —— 简单的内置 hook
    /// 即便没有描述也能注册。
    fn description(&self) -> &str {
        ""
    }

    /// 该 hook 关心的事件类型。引擎会跳过那些 `events()` 不包含
    /// 当前事件种类的 hook。
    fn events(&self) -> &[HookEventKind];

    /// 处理事件。默认返回 `Allow`。
    async fn handle(&self, _event: &HookEvent) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Allow)
    }
}

/// hook 级别错误。引擎默认将 hook 错误视为 `Deny`(fail-closed,
/// 详见 `docs/tools-and-hooks.md` 验收准则)。
#[derive(Debug, Error)]
pub enum HookError {
    #[error("{0}")]
    Other(String),
}

impl From<String> for HookError {
    fn from(s: String) -> Self {
        HookError::Other(s)
    }
}

impl From<&str> for HookError {
    fn from(s: &str) -> Self {
        HookError::Other(s.to_string())
    }
}
