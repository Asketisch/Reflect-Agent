//! `search_budget` —— PreToolUse hook,限制每 turn 内搜索类工具的调用
//! 次数(默认 20)。
//!
//! 参见 `docs/tools-and-hooks.md §5.1`。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;

use crate::decision::HookDecision;
use crate::event::{HookEvent, HookEventKind};
use crate::hook::Hook;

/// PreToolUse 钩子,在 `search_tools` 中的任一工具累计调用达 `max_calls` 次后拒绝执行。
pub struct SearchBudgetHook {
    max_calls: u32,
    counter: AtomicU32,
    search_tools: HashSet<String>,
}

impl SearchBudgetHook {
    /// Default: 20 calls, {grep, glob, read}.
    #[allow(clippy::should_implement_trait)] // pre-M5: kept for back-compat
    pub fn default() -> Self {
        Self::new(
            20,
            ["grep", "glob", "read"]
                .into_iter()
                .map(String::from)
                .collect(),
        )
    }

    pub fn new(max_calls: u32, search_tools: HashSet<String>) -> Self {
        Self {
            max_calls,
            counter: AtomicU32::new(0),
            search_tools,
        }
    }

    /// 重置计数器(测试辅助 / turn 结束钩子用)。
    pub fn reset(&self) {
        self.counter.store(0, Ordering::SeqCst);
    }

    /// Current count (test helper).
    pub fn count(&self) -> u32 {
        self.counter.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Hook for SearchBudgetHook {
    fn name(&self) -> &str {
        "search_budget"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        if let HookEvent::PreToolUse { tool, .. } = event {
            if self.search_tools.contains(tool) {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                if n >= self.max_calls {
                    return Ok(HookDecision::Deny {
                        reason: format!(
                            "Search budget exceeded ({}/{}). Stop searching and use existing information.",
                            n + 1,
                            self.max_calls
                        ),
                    });
                }
            }
        }
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{HookContext, StopReason};
    use reflect_protocol::{PermissionMode, ThreadId, TurnId};
    use std::path::PathBuf;

    fn pre_event(tool: &str) -> HookEvent {
        HookEvent::PreToolUse {
            tool: tool.into(),
            args: serde_json::json!({}),
            ctx: HookContext {
                session_id: ThreadId::new(),
                turn_id: TurnId::new(),
                workspace: PathBuf::from("/"),
                permission_mode: PermissionMode::Auto,
            },
        }
    }

    #[tokio::test]
    async fn allows_below_budget() {
        let h = SearchBudgetHook::new(3, ["grep".into()].into_iter().collect());
        assert_eq!(
            h.handle(&pre_event("grep")).await.unwrap(),
            HookDecision::Allow
        );
        assert_eq!(
            h.handle(&pre_event("grep")).await.unwrap(),
            HookDecision::Allow
        );
    }

    #[tokio::test]
    async fn denies_at_budget() {
        let h = SearchBudgetHook::new(2, ["grep".into()].into_iter().collect());
        let _ = h.handle(&pre_event("grep")).await.unwrap();
        let _ = h.handle(&pre_event("grep")).await.unwrap();
        let d = h.handle(&pre_event("grep")).await.unwrap();
        match d {
            HookDecision::Deny { reason } => assert!(reason.contains("budget")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_non_search_tools() {
        let h = SearchBudgetHook::new(1, ["grep".into()].into_iter().collect());
        for _ in 0..5 {
            assert_eq!(
                h.handle(&pre_event("bash")).await.unwrap(),
                HookDecision::Allow
            );
        }
        assert_eq!(h.count(), 0);
    }

    #[tokio::test]
    async fn ignores_non_pre_tool_use_events() {
        let h = SearchBudgetHook::new(1, ["grep".into()].into_iter().collect());
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }
}
