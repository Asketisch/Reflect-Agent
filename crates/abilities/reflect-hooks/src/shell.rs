//! v1.5 R3:`ShellHook` —— 外部命令式 hook。
//!
//! 把 hook 逻辑委托给外部进程(Claude Code 的 shell command hooks 对
//! 等物):事件序列化为 JSON 写入子进程 stdin,进程以 shell 执行;
//! 决策经 stdout JSON 回传,支持与 Claude Code 兼容的形态:
//!
//! - `{"decision":"deny","reason":"..."}` / `"block"` → [`HookDecision::Deny`]
//! - `{"decision":"ask","reason":"..."}` → [`HookDecision::Ask`]
//! - `{"decision":"allow"}` 或空输出 / 非 JSON 输出 → [`HookDecision::Allow`]
//!   (exit 0 的普通文本输出视为信息性,不阻断)
//!
//! 失败语义(fail-closed):命令超时或非零退出 → [`HookDecision::Deny`]
//! (原因取 stderr/stdout 摘要)—— 外部守卫不可用时宁可拒绝执行。
//!
//! matcher:仅对携带工具名的事件(`PreToolUse` / `PostToolUse`)生效,
//! 支持 `*` / `?` 通配与精确名(大小写不敏感);不匹配 → 直接 Allow,
//! 不启动子进程。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

use crate::decision::{HookDecision, SystemMessage};
use crate::event::{HookEvent, HookEventKind};
use crate::hook::Hook;
use crate::hook::HookError;

/// 默认命令超时(与 Claude Code 的 60s 对齐)。
pub const DEFAULT_SHELL_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// 外部命令式 hook。
#[derive(Debug, Clone)]
pub struct ShellHook {
    name: String,
    kind: HookEventKind,
    matcher: Option<String>,
    command: String,
    timeout: Duration,
}

impl ShellHook {
    pub fn new(
        name: impl Into<String>,
        kind: HookEventKind,
        matcher: Option<String>,
        command: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            matcher,
            command: command.into(),
            timeout,
        }
    }

    /// 从 hooks.json 的事件名字符串解析事件种类(大小写不敏感)。
    /// 未知事件名返回 `None`(调用方 skip + warn)。
    pub fn parse_kind(event: &str) -> Option<HookEventKind> {
        match event.to_ascii_lowercase().as_str() {
            "pretooluse" => Some(HookEventKind::PreToolUse),
            "posttooluse" => Some(HookEventKind::PostToolUse),
            "posttoolusefailure" => Some(HookEventKind::PostToolUseFailure),
            "stop" => Some(HookEventKind::Stop),
            "sessionstart" => Some(HookEventKind::SessionStart),
            "userpromptsubmit" => Some(HookEventKind::UserPromptSubmit),
            "precompact" => Some(HookEventKind::PreCompact),
            "taskcreated" => Some(HookEventKind::TaskCreated),
            "taskcompleted" => Some(HookEventKind::TaskCompleted),
            "taskupdated" => Some(HookEventKind::TaskUpdated),
            _ => None,
        }
    }

    /// matcher 是否放行该工具名。`None` matcher = 全放行。
    fn matcher_allows(&self, tool: Option<&str>) -> bool {
        let Some(pattern) = self.matcher.as_deref() else {
            return true;
        };
        let Some(tool) = tool else {
            return false;
        };
        wildcard_match(pattern, tool)
    }

    /// 执行外部命令:事件 JSON → stdin,读 stdout/stderr,分类决策。
    async fn run_command(&self, event_json: String) -> HookDecision {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(hook = %self.name, error = %e, "shell hook spawn failed; deny");
                return HookDecision::Deny {
                    reason: format!("hook '{}' spawn failed: {e}", self.name),
                };
            }
        };
        let stdin_handle = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // stdin:事件 JSON(写完即关,让 `cat` 类命令能收到 EOF)。
        // review 修复:写入必须并入 timeout 作用域 —— hook 命令不读
        // stdin 且事件 JSON 超过 pipe 缓冲(约 64KB,PreToolUse 的 args
        // 可含整份文件内容)时,无保护的 `write_all` 会永久阻塞,hook
        // 脱离超时保护卡死整个引擎事件派发。
        let collect = async {
            let in_fut = async {
                if let Some(mut stdin) = stdin_handle {
                    use tokio::io::AsyncWriteExt;
                    if let Err(e) = stdin.write_all(event_json.as_bytes()).await {
                        tracing::debug!(
                            hook = %self.name,
                            error = %e,
                            "shell hook stdin write failed"
                        );
                    }
                    let _ = stdin.shutdown().await;
                }
            };
            let out_fut = async {
                let mut buf = String::new();
                if let Some(mut s) = stdout {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut buf).await;
                }
                buf
            };
            let err_fut = async {
                let mut buf = String::new();
                if let Some(mut s) = stderr {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut buf).await;
                }
                buf
            };
            let ((), out, err) = tokio::join!(in_fut, out_fut, err_fut);
            let status = child.wait().await;
            (out, err, status)
        };

        let (out, err, status) = match tokio::time::timeout(self.timeout, collect).await {
            Ok(r) => r,
            Err(_) => {
                let _ = child.start_kill();
                tracing::warn!(
                    hook = %self.name,
                    timeout_ms = self.timeout.as_millis() as u64,
                    "shell hook timed out; deny (fail-closed)"
                );
                return HookDecision::Deny {
                    reason: format!(
                        "hook '{}' timed out after {}ms",
                        self.name,
                        self.timeout.as_millis()
                    ),
                };
            }
        };

        let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        if exit_code != 0 {
            let excerpt: String = {
                let mut e = if err.trim().is_empty() {
                    out.clone()
                } else {
                    err.clone()
                };
                e.truncate(200);
                e
            };
            tracing::warn!(
                hook = %self.name,
                exit_code,
                "shell hook exited non-zero; deny (fail-closed)"
            );
            return HookDecision::Deny {
                reason: format!("hook '{}' failed (exit {exit_code}): {excerpt}", self.name),
            };
        }

        // exit 0:解析 stdout JSON 决策;空 / 非 JSON → Allow(信息性输出)。
        let trimmed = out.trim();
        if trimmed.is_empty() {
            return HookDecision::Allow;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => {
                let decision = v
                    .get("decision")
                    .and_then(|d| d.as_str())
                    .unwrap_or("allow")
                    .to_ascii_lowercase();
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_string();
                match decision.as_str() {
                    "deny" | "block" => HookDecision::Deny {
                        reason: if reason.is_empty() {
                            format!("denied by hook '{}'", self.name)
                        } else {
                            reason
                        },
                    },
                    "ask" => HookDecision::Ask {
                        reason: if reason.is_empty() {
                            format!("asked by hook '{}'", self.name)
                        } else {
                            reason
                        },
                    },
                    "inject" | "message" => HookDecision::InjectMessage(SystemMessage {
                        content: if reason.is_empty() {
                            trimmed.to_string()
                        } else {
                            reason
                        },
                    }),
                    // allow / ok / 未知值 → 放行(保守:hook 未表达拒绝)。
                    _ => HookDecision::Allow,
                }
            }
            Err(_) => {
                tracing::debug!(
                    hook = %self.name,
                    stdout = %trimmed,
                    "shell hook stdout is not JSON; treating as informational (allow)"
                );
                HookDecision::Allow
            }
        }
    }
}

#[async_trait]
impl Hook for ShellHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "external command hook"
    }

    fn events(&self) -> &[HookEventKind] {
        std::slice::from_ref(&self.kind)
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        // matcher:仅对携带工具名的事件生效。
        let tool = match event {
            HookEvent::PreToolUse { tool, .. } => Some(tool.as_str()),
            HookEvent::PostToolUse { tool, .. } => Some(tool.as_str()),
            _ => None,
        };
        if !self.matcher_allows(tool) {
            return Ok(HookDecision::Allow);
        }
        let json = serde_json::to_string(event)
            .map_err(|e| HookError::Other(format!("hook event serialize failed: {e}")))?;
        Ok(self.run_command(json).await)
    }
}

/// 极简通配匹配:`*` 任意序列、`?` 单字符,其余按字面(大小写不敏感)。
/// 供 matcher 用;避免为一个小特性引入 globset 依赖。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => {
                // `*` 匹配空或吞一个字符(回溯)。
                inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..]))
            }
            (Some('?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(a), Some(b)) => a.to_lowercase().eq(b.to_lowercase()) && inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let pc: Vec<char> = pattern.chars().collect();
    let tc: Vec<char> = text.chars().collect();
    inner(&pc, &tc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre_tool_event(tool: &str) -> HookEvent {
        serde_json::from_value(serde_json::json!({
            "kind": "pre_tool_use",
            "tool": tool,
            "args": {},
            "ctx": {
                "session_id": "00000000-0000-0000-0000-000000000000",
                "turn_id": "00000000-0000-0000-0000-000000000000",
                "workspace": "/tmp",
                "permission_mode": "auto"
            }
        }))
        .unwrap()
    }

    #[test]
    fn wildcard_matches_glob_and_exact() {
        assert!(wildcard_match("bash", "bash"));
        assert!(wildcard_match("BASH", "bash"), "大小写不敏感");
        assert!(wildcard_match("mcp__*", "mcp__github"));
        assert!(wildcard_match("web_*", "web_search"));
        assert!(!wildcard_match("bash", "bashx"));
        assert!(!wildcard_match("bash", "echo"));
        assert!(wildcard_match("tool?", "tools"));
        assert!(!wildcard_match("tool?", "tool"));
    }

    /// `cat`:回显 stdin(exit 0、非 JSON)→ Allow。
    #[tokio::test]
    async fn informational_output_allows() {
        let hook = ShellHook::new(
            "audit",
            HookEventKind::PreToolUse,
            None,
            "cat",
            DEFAULT_SHELL_HOOK_TIMEOUT,
        );
        let ev = pre_tool_event("bash");
        let d = hook.handle(&ev).await.unwrap();
        assert!(matches!(d, HookDecision::Allow));
    }

    /// stdout JSON deny → Deny(带 reason)。
    #[tokio::test]
    async fn json_decision_deny() {
        let hook = ShellHook::new(
            "policy",
            HookEventKind::PreToolUse,
            Some("bash".to_string()),
            "echo '{\"decision\":\"deny\",\"reason\":\"blocked by policy\"}'",
            DEFAULT_SHELL_HOOK_TIMEOUT,
        );
        let ev = pre_tool_event("bash");
        let d = hook.handle(&ev).await.unwrap();
        match d {
            HookDecision::Deny { reason } => assert_eq!(reason, "blocked by policy"),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    /// ask 决策 → Ask(交给审批门)。
    #[tokio::test]
    async fn json_decision_ask() {
        let hook = ShellHook::new(
            "guard",
            HookEventKind::PreToolUse,
            None,
            "echo '{\"decision\":\"ask\",\"reason\":\"confirm?\"}'",
            DEFAULT_SHELL_HOOK_TIMEOUT,
        );
        let ev = pre_tool_event("bash");
        assert!(matches!(
            hook.handle(&ev).await.unwrap(),
            HookDecision::Ask { .. }
        ));
    }

    /// 非零退出 → fail-closed Deny。
    #[tokio::test]
    async fn nonzero_exit_denies() {
        let hook = ShellHook::new(
            "bad",
            HookEventKind::PreToolUse,
            None,
            "echo boom >&2; exit 3",
            DEFAULT_SHELL_HOOK_TIMEOUT,
        );
        let ev = pre_tool_event("bash");
        match hook.handle(&ev).await.unwrap() {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("exit 3"), "reason: {reason}");
                assert!(reason.contains("boom"), "应带 stderr 摘要: {reason}");
            }
            other => panic!("expected deny, got {other:?}"),
        }
    }

    /// 超时 → fail-closed Deny。
    #[tokio::test]
    async fn timeout_denies() {
        let hook = ShellHook::new(
            "stuck",
            HookEventKind::PreToolUse,
            None,
            "sleep 5",
            Duration::from_millis(150),
        );
        let ev = pre_tool_event("bash");
        match hook.handle(&ev).await.unwrap() {
            HookDecision::Deny { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    /// matcher 不匹配 → 不执行命令直接 Allow(命令会 deny,但没跑)。
    #[tokio::test]
    async fn matcher_mismatch_skips_command() {
        let hook = ShellHook::new(
            "only-bash",
            HookEventKind::PreToolUse,
            Some("bash".to_string()),
            "echo '{\"decision\":\"deny\"}'",
            DEFAULT_SHELL_HOOK_TIMEOUT,
        );
        let ev = pre_tool_event("read");
        assert!(matches!(
            hook.handle(&ev).await.unwrap(),
            HookDecision::Allow
        ));
    }

    /// parse_kind:事件名映射与未知名。
    #[test]
    fn parse_kind_variants() {
        assert_eq!(
            ShellHook::parse_kind("PreToolUse"),
            Some(HookEventKind::PreToolUse)
        );
        assert_eq!(
            ShellHook::parse_kind("stop"),
            Some(HookEventKind::Stop),
            "大小写不敏感"
        );
        assert_eq!(ShellHook::parse_kind("Nonsense"), None);
    }
}
