//! `verification` —— Stop hook,运行 shell 命令(如 `cargo test`),
//! 若失败则否决 turn 完成。
//!
//! 参见 `docs/tools-and-hooks.md §5.4`。
//!
//! M3 v0:直接调用 `tokio::process::Command`(不经过 `ToolExecutionQueue`,
//! 以避免 hook 的递归派发)。最长 5 分钟。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::decision::{HookDecision, SystemMessage};
use crate::event::{HookEvent, HookEventKind, StopReason};
use crate::hook::Hook;

const VERIFY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_ATTEMPTS: u32 = 2;
const OUTPUT_LIMIT: usize = 100 * 1024;

pub struct VerificationHook {
    run_on_stop: bool,
    test_command: String,
}

impl VerificationHook {
    pub fn new(run_on_stop: bool, test_command: impl Into<String>) -> Self {
        Self {
            run_on_stop,
            test_command: test_command.into(),
        }
    }
}

impl Default for VerificationHook {
    fn default() -> Self {
        Self::new(true, "cargo test")
    }
}

#[async_trait]
impl Hook for VerificationHook {
    fn name(&self) -> &str {
        "verification"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::Stop]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        if !self.run_on_stop {
            return Ok(HookDecision::Allow);
        }
        if let HookEvent::Stop { reason, attempt } = event {
            if matches!(reason, StopReason::AgentDecision) && *attempt < MAX_ATTEMPTS {
                let result = run_command(&self.test_command).await;
                if !result.passed {
                    let summary = format!(
                        "Tests failed ({}). Last 30 lines:\n{}",
                        result.command, result.tail
                    );
                    return Ok(HookDecision::Combined(vec![
                        HookDecision::InjectMessage(SystemMessage::new(summary)),
                        HookDecision::Deny {
                            reason: "tests failing".into(),
                        },
                    ]));
                }
            }
        }
        Ok(HookDecision::Allow)
    }
}

struct CommandResult {
    command: String,
    passed: bool,
    tail: String,
}

async fn run_command(cmd: &str) -> CommandResult {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                command: cmd.to_string(),
                passed: false,
                tail: format!("spawn error: {e}"),
            };
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::with_capacity(8192);
    let mut err_buf = Vec::with_capacity(8192);
    let collect = async {
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(OUTPUT_LIMIT as u64).read_to_end(&mut out_buf).await;
        }
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(OUTPUT_LIMIT as u64).read_to_end(&mut err_buf).await;
        }
        child.wait().await
    };
    let exit = timeout(VERIFY_TIMEOUT, collect).await;
    let passed = match exit {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            return CommandResult {
                command: cmd.to_string(),
                passed: false,
                tail: format!("wait error: {e}"),
            };
        }
        Err(_) => {
            return CommandResult {
                command: cmd.to_string(),
                passed: false,
                tail: format!("timeout after {}s", VERIFY_TIMEOUT.as_secs()),
            };
        }
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out_buf),
        String::from_utf8_lossy(&err_buf)
    );
    // 为摘要保留最后 30 行。
    let tail: String = combined
        .lines()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    CommandResult {
        command: cmd.to_string(),
        passed,
        tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_allows() {
        let h = VerificationHook::new(false, "false");
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn successful_command_allows() {
        let h = VerificationHook::new(true, "true");
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn failing_command_denies_with_summary() {
        let h = VerificationHook::new(true, "echo FAILED && exit 1");
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        let d = h.handle(&e).await.unwrap();
        match d {
            HookDecision::Combined(parts) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    HookDecision::InjectMessage(m) => assert!(m.content.contains("FAILED")),
                    other => panic!("expected inject, got {other:?}"),
                }
                assert!(matches!(parts[1], HookDecision::Deny { .. }));
            }
            other => panic!("expected combined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_after_max_attempts() {
        let h = VerificationHook::new(true, "false");
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: MAX_ATTEMPTS,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }
}
