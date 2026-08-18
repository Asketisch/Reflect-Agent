//! `plan_completion` —— Stop hook,在允许 turn 以 `AgentDecision` 原因
//! 结束前要求显式验证。
//!
//! 参见 `docs/tools-and-hooks.md §5.3`。

use async_trait::async_trait;

use crate::decision::{HookDecision, SystemMessage};
use crate::event::{HookEvent, HookEventKind, StopReason};
use crate::hook::Hook;

const MAX_ATTEMPTS: u32 = 3;

pub struct PlanCompletionHook {
    strict: bool,
}

impl PlanCompletionHook {
    pub fn new(strict: bool) -> Self {
        Self { strict }
    }
}

impl Default for PlanCompletionHook {
    fn default() -> Self {
        Self::new(true)
    }
}

#[async_trait]
impl Hook for PlanCompletionHook {
    fn name(&self) -> &str {
        "plan_completion"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::Stop]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        if let HookEvent::Stop { reason, attempt } = event {
            if matches!(reason, StopReason::AgentDecision) {
                if self.strict && *attempt < MAX_ATTEMPTS {
                    return Ok(HookDecision::Combined(vec![
                        HookDecision::InjectMessage(SystemMessage::new(
                            "Before finishing, verify: Did you address ALL user \
                             requirements? Are there unverified claims? Reply with \
                             explicit confirmation.",
                        )),
                        HookDecision::Deny {
                            reason: "Verification required".into(),
                        },
                    ]));
                }
            }
        }
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_event(attempt: u32) -> HookEvent {
        HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt,
        }
    }

    #[tokio::test]
    async fn strict_denies_first_attempt() {
        let h = PlanCompletionHook::new(true);
        let d = h.handle(&stop_event(0)).await.unwrap();
        match d {
            HookDecision::Combined(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], HookDecision::InjectMessage(_)));
                assert!(matches!(parts[1], HookDecision::Deny { .. }));
            }
            other => panic!("expected combined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_allows_after_max_attempts() {
        let h = PlanCompletionHook::new(true);
        let d = h.handle(&stop_event(MAX_ATTEMPTS)).await.unwrap();
        assert_eq!(d, HookDecision::Allow);
    }

    #[tokio::test]
    async fn non_strict_always_allows() {
        let h = PlanCompletionHook::new(false);
        assert_eq!(h.handle(&stop_event(0)).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn non_agent_decision_always_allows() {
        let h = PlanCompletionHook::new(true);
        let e = HookEvent::Stop {
            reason: StopReason::MaxIterations,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }
}
