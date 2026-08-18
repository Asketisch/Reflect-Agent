//! `test_runner` —— PostToolUse hook,监测 `bash` 工具输出中的测试失败
//! 特征,并注入 `<system-reminder>` 要求模型修复根本原因。
//!
//! 参见 `docs/tools-and-hooks.md §5.2`。

use async_trait::async_trait;

use crate::decision::{HookDecision, SystemMessage};
use crate::event::{HookEvent, HookEventKind};
use crate::hook::Hook;

const FAILURE_PATTERNS: &[&str] = &[
    "test result: FAILED",
    "FAILED",
    "panicked at",
    "error[E", // rustc
    "error: ", // cargo
    "AssertionError",
    "Traceback (most recent call last)",
];

/// PostToolUse hook for bash test failures.
pub struct TestRunnerHook {
    enabled: bool,
}

impl TestRunnerHook {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl Default for TestRunnerHook {
    fn default() -> Self {
        Self::new(true)
    }
}

fn extract_text(content: &[reflect_protocol::ContentBlock]) -> String {
    let mut out = String::new();
    for c in content {
        if let reflect_protocol::ContentBlock::Text { text } = c {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

fn looks_like_test_failure(output: &str) -> bool {
    FAILURE_PATTERNS.iter().any(|p| output.contains(p))
}

#[async_trait]
impl Hook for TestRunnerHook {
    fn name(&self) -> &str {
        "test_runner"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PostToolUse]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        if !self.enabled {
            return Ok(HookDecision::Allow);
        }
        if let HookEvent::PostToolUse { tool, result, .. } = event {
            if tool == "bash" && !result.is_error {
                let text = extract_text(&result.content);
                if looks_like_test_failure(&text) {
                    return Ok(HookDecision::InjectMessage(SystemMessage::new(
                        "<system-reminder>\n\
                         Test failure detected. Analyze the failure and fix the root \
                         cause before proceeding.\n\
                         </system-reminder>",
                    )));
                }
            }
        }
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{ContentBlock, ToolOutput};

    fn post_bash_event(text: &str) -> HookEvent {
        HookEvent::PostToolUse {
            tool: "bash".into(),
            result: ToolOutput {
                content: vec![ContentBlock::text(text)],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 0,
            },
            elapsed_ms: 0,
        }
    }

    #[tokio::test]
    async fn detects_test_result_failed() {
        let h = TestRunnerHook::default();
        let d = h
            .handle(&post_bash_event("test result: FAILED. 3 passed; 5 failed"))
            .await
            .unwrap();
        assert!(matches!(d, HookDecision::InjectMessage(_)));
    }

    #[tokio::test]
    async fn detects_rustc_error() {
        let h = TestRunnerHook::default();
        let d = h
            .handle(&post_bash_event("error[E0432]: unresolved import"))
            .await
            .unwrap();
        assert!(matches!(d, HookDecision::InjectMessage(_)));
    }

    #[tokio::test]
    async fn detects_traceback() {
        let h = TestRunnerHook::default();
        let d = h
            .handle(&post_bash_event(
                "Traceback (most recent call last):\n  File ...",
            ))
            .await
            .unwrap();
        assert!(matches!(d, HookDecision::InjectMessage(_)));
    }

    #[tokio::test]
    async fn success_output_does_not_trigger() {
        let h = TestRunnerHook::default();
        let d = h
            .handle(&post_bash_event("test result: ok. 5 passed; 0 failed"))
            .await
            .unwrap();
        assert_eq!(d, HookDecision::Allow);
    }

    #[tokio::test]
    async fn non_bash_tool_does_not_trigger() {
        let h = TestRunnerHook::default();
        let e = HookEvent::PostToolUse {
            tool: "read".into(),
            result: ToolOutput {
                content: vec![ContentBlock::text("FAILED: this is just text")],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 0,
            },
            elapsed_ms: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn disabled_hook_always_allows() {
        let h = TestRunnerHook::new(false);
        let d = h
            .handle(&post_bash_event("test result: FAILED"))
            .await
            .unwrap();
        assert_eq!(d, HookDecision::Allow);
    }
}
