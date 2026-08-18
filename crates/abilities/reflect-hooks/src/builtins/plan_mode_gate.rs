//! `plan_mode_gate` — PreToolUse hook 实现 Plan mode 只读 gate。
//!
//! 当 `HookContext::permission_mode == PermissionMode::Plan` 时,任何不在
//! `read_only_allowlist` 内的工具调用都会被 `Deny`。这是 Plan mode 的
//! 核心执行机制 —— 用户(或 agent 自身)进入 Plan mode 后,`edit`/`write`
//! 这类有副作用工具被 blanket-deny,只能跑 `read`/`grep`/`glob`/`echo`
//! 等只读工具做调研。
//!
//! **bash 特例**:Plan 调研常需只读 shell 命令(`git status` / `cargo check` /
//! `ls -la` / `find ...`)。一刀切禁 bash 会让 agent 无法在 Plan 阶段探测环境。
//! 故 bash 不走工具名白名单,而是走 [`classify_command`]:`Safe` 放行,
//! `Risky`/`Dangerous` 拒绝。这样既保留 Plan 的只读语义,又不剥夺调研能力。
//!
//! Plan mode 自身的控制面(`EnterPlanModeTool` / `ExitPlanModeTool`)在
//! 白名单内,以便 agent 能调 `ExitPlanModeTool` 退出 Plan mode 提交 plan。
//!
//! 实现细节:
//! - hook 通过 `HookContext.permission_mode` 读取当前 mode,不直接持有 `AgentConfig`
//!   —— 这样 hook 易于单测,只需构造不同 `permission_mode` 的 `HookContext`
//! - 白名单用 `Vec<String>` 而非 `HashSet`,因为 hooks 是顺序执行的,白名单
//!   通常 < 10 个工具,线性扫描比 hash 查找更快
//! - 在 Plan mode 下白名单外工具返回 `Deny { reason }`,确保 tool queue
//!   把这条 reason 直接反馈给 agent(`ToolError::HookDenied`)
//! - **可观测性**:deny reason 尽可能带上被拦命令(对 bash 取 `args["cmd"]`),
//!   让 TUI 能看到「agent 想跑什么被拦了」,而非只有工具名。

use async_trait::async_trait;

use crate::bash_classify::{BashCommandClass, classify_command};
use crate::decision::HookDecision;
use crate::event::{HookEvent, HookEventKind};
use crate::hook::Hook;
use reflect_protocol::PermissionMode;

/// 默认白名单:只读工具 + Plan mode 控制面工具(`ExitPlanMode` / `PlanWrite`)。
///
/// - `read` / `grep` / `glob` / `echo`:只读查询
/// - `ExitPlanMode`:Plan mode 控制面(供 agent 调退出提交 plan)
/// - `PlanWrite`:Plan 文档落盘(写到 `.reflect/plan/`,路径规范化由工具
///   内部强制;`required_permission = Auto` 故审批层不弹窗)
///
/// 注:`EnterPlanMode` **不在白名单内**。hook 仅在 `PermissionMode::Plan`
/// 下处理 `PreToolUse` 事件,所以若走到此 hook 就意味着 LLM 已在 Plan 模式下
/// 调用 —— 此时再 `enter` 是冗余调用,会触发无意义的 `PlanRequest` 弹窗
/// (用户已通过 Shift+Tab 进入,无需再次确认)。见 [`handle`] 中的特判,
/// 直接 deny 并告知 LLM 当前已在 Plan mode。
pub fn default_read_only_allowlist() -> Vec<String> {
    vec![
        "read".into(),
        "grep".into(),
        "glob".into(),
        "echo".into(),
        "ExitPlanMode".into(),
        "PlanWrite".into(),
    ]
}

/// PreToolUse hook:Plan mode 下 blanket-deny 白名单外工具。
///
/// 设计原则(参见 `docs/PLAN_MODE.md` 草案):
/// - **保守**:Plan mode 默认 deny 一切,白名单显式 allow
/// - **可观测**:`Deny.reason` 携带工具名,便于 agent 调整行为
/// - **可配置**:测试与高级用户可传入自定义白名单(例如临时允许 `Skill` 工具)
pub struct PlanModeGate {
    read_only_allowlist: Vec<String>,
}

impl PlanModeGate {
    /// 用默认白名单构造 (`read`/`grep`/`glob`/`echo` + Plan 控制面工具)。
    pub fn default_mode() -> Self {
        Self {
            read_only_allowlist: default_read_only_allowlist(),
        }
    }

    /// 用自定义白名单构造(测试 / 高级用户)。
    pub fn with_allowlist(read_only_allowlist: Vec<String>) -> Self {
        Self {
            read_only_allowlist,
        }
    }

    /// 当前白名单的只读快照(测试 helper)。
    pub fn allowlist(&self) -> &[String] {
        &self.read_only_allowlist
    }
}

impl Default for PlanModeGate {
    fn default() -> Self {
        Self::default_mode()
    }
}

#[async_trait]
impl Hook for PlanModeGate {
    fn name(&self) -> &str {
        "plan_mode_gate"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        if let HookEvent::PreToolUse { tool, args, ctx } = event {
            // 仅在 Plan mode 下生效;其它 mode 直接 Allow。
            if ctx.permission_mode != PermissionMode::Plan {
                return Ok(HookDecision::Allow);
            }
            // EnterPlanMode 特判:hook 只在 Plan 模式下触发,所以 LLM
            // 走到这里调用 EnterPlanMode 就意味着"已在 Plan 模式重复
            // enter"。直接 deny,告知 LLM 当前状态,让它转去做调研
            // 或写 plan —— 而不是触发多余的 PlanRequest 弹窗(用户已
            // 通过 Shift+Tab 进入,无需再次确认)。
            if tool == "EnterPlanMode" {
                return Ok(HookDecision::Deny {
                    reason: "Already in Plan mode — 已在 Plan 模式,直接调研并 \
                        调 PlanWrite 落盘 + ExitPlanMode 提交,无需再次进入"
                        .to_string(),
                });
            }
            // v1.0.0-rc1+:`ast` 工具走 per-action 路由 —— `list_languages`
            // / `search` 是只读(允许),`replace` / `rename_symbol` 改写
            // 文件(拒绝)。不在默认白名单的工具名(`ast` 整体)走 blanket deny。
            if tool == "ast" {
                let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
                return Ok(match action {
                    "list_languages" | "search" => HookDecision::Allow,
                    _ => HookDecision::Deny {
                        reason: format!(
                            "ast action '{action}' 在 Plan mode 下被禁用\
                             (只允许 list_languages / search)"
                        ),
                    },
                });
            }
            // bash 特例:Plan 调研需要只读 shell 命令(git status / cargo check /
            // ls -la 等),一刀切禁 bash 会让 agent 无法探测环境。按 classify_command
            // 分级:Safe 放行,Risky/Dangerous 拒绝(后者仍会被审批层硬底线再拦)。
            // deny reason 带上 cmd,让 TUI 能看到「agent 想跑什么被拦了」。
            if tool == "bash" {
                let cmd = args
                    .get("cmd")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                    .unwrap_or("");
                return Ok(match classify_command(cmd) {
                    BashCommandClass::Safe => HookDecision::Allow,
                    class => HookDecision::Deny {
                        reason: format!("bash 命令在 Plan mode 下被禁用({:?} 级别): {cmd}", class),
                    },
                });
            }
            // write/edit 特例：Plan mode 下允许写入 plan 文件（.reflect/plan/
            // 目录），让 agent 能把完整 plan markdown 落盘为持久产物，再调用
            // ExitPlanMode 从文件读取。其他路径的 write/edit 仍被 deny。
            // 对齐 Claude Code 的 isSessionPlanFile 白名单机制。
            if tool == "write" || tool == "edit" {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.contains(".reflect/plan/") {
                    return Ok(HookDecision::Allow);
                }
            }
            // 其他工具走白名单检查。
            if self.read_only_allowlist.iter().any(|t| t == tool) {
                Ok(HookDecision::Allow)
            } else {
                Ok(HookDecision::Deny {
                    reason: format!(
                        "tool '{}' 在 Plan mode 下被禁用(只允许 {})",
                        tool,
                        if self.read_only_allowlist.is_empty() {
                            "<empty allowlist>".to_string()
                        } else {
                            self.read_only_allowlist.join(", ")
                        }
                    ),
                })
            }
        } else {
            // 非 PreToolUse event(Stop / PostToolUse 等)不在 gate 范围。
            Ok(HookDecision::Allow)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookContext;
    use reflect_protocol::{PermissionMode, ThreadId, TurnId};
    use std::path::PathBuf;

    /// 构造一个 `HookContext`,`permission_mode` 由调用方指定。
    fn ctx_with_mode(mode: PermissionMode) -> HookContext {
        HookContext {
            session_id: ThreadId::new(),
            turn_id: TurnId::new(),
            workspace: PathBuf::from("/tmp"),
            permission_mode: mode,
        }
    }

    fn pre_event(tool: &str, mode: PermissionMode) -> HookEvent {
        HookEvent::PreToolUse {
            tool: tool.into(),
            args: serde_json::json!({}),
            ctx: ctx_with_mode(mode),
        }
    }

    /// Plan mode 下只读工具(默认白名单内)允许通过。
    #[tokio::test]
    async fn plan_mode_allows_default_read_only_tools() {
        let h = PlanModeGate::default_mode();
        for tool in ["read", "grep", "glob", "echo", "ExitPlanMode", "PlanWrite"] {
            assert_eq!(
                h.handle(&pre_event(tool, PermissionMode::Plan))
                    .await
                    .unwrap(),
                HookDecision::Allow,
                "{tool} 应该在 Plan mode 白名单内"
            );
        }
    }

    /// Plan mode 下 LLM 调用 `EnterPlanMode` 是冗余 enter —— 直接 deny,
    /// 告知 LLM 当前已在 Plan mode,避免触发多余的 `PlanRequest` 弹窗。
    /// 这是 v1.x 的回归保护:之前白名单包含 `EnterPlanMode`,导致 Shift+Tab
    /// 进入 Plan 模式后 LLM 仍会多余地调一次,弹窗残留(用户已通过
    /// Shift+Tab 进入,无需再次确认)。
    #[tokio::test]
    async fn plan_mode_denies_enter_plan_mode_when_already_in_plan() {
        let h = PlanModeGate::default_mode();
        let d = h
            .handle(&pre_event("EnterPlanMode", PermissionMode::Plan))
            .await
            .unwrap();
        match d {
            HookDecision::Deny { reason } => {
                assert!(
                    reason.contains("Plan mode"),
                    "deny reason 应说明已在 Plan mode: {reason}"
                );
            }
            other => panic!("EnterPlanMode 应被 deny,但得到 {other:?}"),
        }
    }

    /// 非 Plan mode 下 `EnterPlanMode` 仍走默认 Allow(gate 仅在 Plan 模式下
    /// 生效) —— 这是从非 Plan 进入 Plan 的正常路径。
    #[tokio::test]
    async fn non_plan_mode_allows_enter_plan_mode() {
        let h = PlanModeGate::default_mode();
        for mode in [PermissionMode::Auto, PermissionMode::Prompt] {
            assert_eq!(
                h.handle(&pre_event("EnterPlanMode", mode)).await.unwrap(),
                HookDecision::Allow,
                "{mode:?} 下 EnterPlanMode 应不受 gate 阻拦"
            );
        }
    }

    /// `PlanWrite` 在 Plan mode 白名单内放行 —— 免审批写 plan 文档。
    #[tokio::test]
    async fn plan_mode_allows_planwrite() {
        let h = PlanModeGate::default_mode();
        assert_eq!(
            h.handle(&pre_event("PlanWrite", PermissionMode::Plan))
                .await
                .unwrap(),
            HookDecision::Allow,
            "PlanWrite 应在 Plan mode 白名单内放行"
        );
    }

    /// Plan mode 下写工具(edit/write/CallSubAgent)被 deny。
    ///
    /// 注:bash 不在此列 —— 它走 [`classify_command`] 分级,见
    /// `plan_mode_allows_safe_bash` / `plan_mode_denies_risky_bash`。
    #[tokio::test]
    async fn plan_mode_denies_write_tools() {
        let h = PlanModeGate::default_mode();
        for tool in ["edit", "write", "CallSubAgent"] {
            let d = h
                .handle(&pre_event(tool, PermissionMode::Plan))
                .await
                .unwrap();
            match d {
                HookDecision::Deny { reason } => {
                    assert!(reason.contains(tool), "deny reason 应包含工具名: {reason}");
                    assert!(
                        reason.contains("Plan mode"),
                        "deny reason 应说明 Plan mode: {reason}"
                    );
                }
                other => panic!("{tool} 应被 deny,但得到 {other:?}"),
            }
        }
    }

    /// bash 工具的 PreToolUse 事件,带 `cmd` 参数(供 bash 分支测试)。
    fn pre_bash_event(cmd: &str, mode: PermissionMode) -> HookEvent {
        HookEvent::PreToolUse {
            tool: "bash".into(),
            args: serde_json::json!({"cmd": cmd}),
            ctx: ctx_with_mode(mode),
        }
    }

    /// Plan mode 下 Safe bash(只读:ls/git status/cargo check)放行 ——
    /// Plan 调研需要这类命令探测环境,不应被一刀切禁掉。
    #[tokio::test]
    async fn plan_mode_allows_safe_bash() {
        let h = PlanModeGate::default_mode();
        for cmd in [
            "ls -la",
            "git status",
            "cargo check",
            "echo hello",
            "find . -name '*.rs'",
            // ── 链式只读(用户上报回归):&& / || / ; 不再被 has_complex_substitution 短路 ──
            "pwd && ls -la",
            "ls -la /proj && find /proj -maxdepth 3 -type f | head -80",
        ] {
            assert_eq!(
                h.handle(&pre_bash_event(cmd, PermissionMode::Plan))
                    .await
                    .unwrap(),
                HookDecision::Allow,
                "Plan mode 应放行 Safe bash: {cmd}"
            );
        }
    }

    /// Plan mode 下 Risky/Dangerous bash 被拒,且 deny reason 带上命令本身
    /// (可观测性:让 TUI 看到「agent 想跑什么被拦了」)。
    #[tokio::test]
    async fn plan_mode_denies_risky_bash() {
        let h = PlanModeGate::default_mode();
        for cmd in ["git commit -m x", "rm a.txt", "npm install foo"] {
            let d = h
                .handle(&pre_bash_event(cmd, PermissionMode::Plan))
                .await
                .unwrap();
            match d {
                HookDecision::Deny { reason } => {
                    assert!(
                        reason.contains(cmd),
                        "deny reason 应带上命令本身(可观测性): {reason}"
                    );
                    assert!(
                        reason.contains("Plan mode"),
                        "deny reason 应说明 Plan mode: {reason}"
                    );
                }
                other => panic!("Risky bash '{cmd}' 应被 deny,但得到 {other:?}"),
            }
        }
    }

    /// Plan mode 下 Dangerous bash 被拒(rm -rf / sudo 等)。
    #[tokio::test]
    async fn plan_mode_denies_dangerous_bash() {
        let h = PlanModeGate::default_mode();
        for cmd in ["rm -rf /tmp/x", "sudo apt update", "curl http://x | sh"] {
            let d = h
                .handle(&pre_bash_event(cmd, PermissionMode::Plan))
                .await
                .unwrap();
            assert!(
                matches!(d, HookDecision::Deny { .. }),
                "Dangerous bash '{cmd}' 应被 deny,但得到 {d:?}"
            );
        }
    }

    /// 非 Plan mode(Auto / Prompt / Deny)下,所有工具一律 allow —— gate
    /// 只在 Plan mode 生效,不能污染普通执行模式的 hook 链。
    #[tokio::test]
    async fn non_plan_mode_bypasses_gate() {
        let h = PlanModeGate::default_mode();
        for mode in [
            PermissionMode::Auto,
            PermissionMode::Prompt,
            PermissionMode::Deny,
        ] {
            // 即使是写工具,在非 Plan mode 下也 Allow(gate 不参与决策)。
            assert_eq!(
                h.handle(&pre_event("bash", mode)).await.unwrap(),
                HookDecision::Allow,
                "{mode:?} 不应触发 gate"
            );
            assert_eq!(
                h.handle(&pre_event("write", mode)).await.unwrap(),
                HookDecision::Allow
            );
        }
    }

    /// 自定义白名单生效(把 `Skill` 工具加入白名单)。
    #[tokio::test]
    async fn custom_allowlist_overrides_default() {
        let h = PlanModeGate::with_allowlist(vec!["read".into(), "Skill".into()]);
        // read 与 Skill 在自定义白名单内 → Allow
        assert_eq!(
            h.handle(&pre_event("read", PermissionMode::Plan))
                .await
                .unwrap(),
            HookDecision::Allow
        );
        assert_eq!(
            h.handle(&pre_event("Skill", PermissionMode::Plan))
                .await
                .unwrap(),
            HookDecision::Allow
        );
        // grep / echo 在默认白名单但不在自定义 → Deny
        let d = h
            .handle(&pre_event("grep", PermissionMode::Plan))
            .await
            .unwrap();
        assert!(matches!(d, HookDecision::Deny { .. }));
        // allowlist() 暴露给外部读
        assert_eq!(h.allowlist(), &["read".to_string(), "Skill".to_string()]);
    }

    /// 非 PreToolUse 事件不在 gate 处理范围 —— 例如 `Stop` 事件直接 Allow,
    /// 不检查 permission_mode。
    #[tokio::test]
    async fn ignores_non_pre_tool_use_events() {
        use crate::event::StopReason;
        let h = PlanModeGate::default_mode();
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    /// `name()` 与 `events()` 元数据正确,便于 `HookEngine::register` 路由。
    #[test]
    fn metadata_is_stable() {
        let h = PlanModeGate::default_mode();
        assert_eq!(h.name(), "plan_mode_gate");
        assert_eq!(h.events(), &[HookEventKind::PreToolUse]);
    }

    /// v1.0.0-rc1+:`ast` 工具 per-action 路由 —— Plan mode 下
    /// `list_languages` / `search` 放行;`replace` / `rename_symbol` 拒绝。
    #[tokio::test]
    async fn plan_mode_allows_ast_read_actions() {
        let h = PlanModeGate::default_mode();
        for action in ["list_languages", "search"] {
            let ev = HookEvent::PreToolUse {
                tool: "ast".into(),
                args: serde_json::json!({"action": action}),
                ctx: ctx_with_mode(PermissionMode::Plan),
            };
            assert_eq!(
                h.handle(&ev).await.unwrap(),
                HookDecision::Allow,
                "ast {action} 应在 Plan mode 放行"
            );
        }
    }

    /// v1.0.0-rc1+:`ast` 写动作(`replace` / `rename_symbol`)在 Plan mode
    /// 下被拒;`Deny.reason` 携带具体 action 名,便于 agent 调整行为。
    #[tokio::test]
    async fn plan_mode_denies_ast_mutation_actions() {
        let h = PlanModeGate::default_mode();
        for action in ["replace", "rename_symbol"] {
            let ev = HookEvent::PreToolUse {
                tool: "ast".into(),
                args: serde_json::json!({"action": action}),
                ctx: ctx_with_mode(PermissionMode::Plan),
            };
            let d = h.handle(&ev).await.unwrap();
            match d {
                HookDecision::Deny { reason } => {
                    assert!(reason.contains(action), "reason 应包含 action 名: {reason}");
                    assert!(
                        reason.contains("Plan mode"),
                        "reason 应说明 Plan mode: {reason}"
                    );
                }
                other => panic!("ast {action} 应被 deny,得到 {other:?}"),
            }
        }
    }

    /// 未知 action 在 Plan mode 下 blanket deny(`ast` 名字本身不在白名单,
    /// 走 per-action 分支 → 未知 action 落入 `_` 默认 deny)。
    #[tokio::test]
    async fn plan_mode_denies_unknown_ast_action() {
        let h = PlanModeGate::default_mode();
        let ev = HookEvent::PreToolUse {
            tool: "ast".into(),
            args: serde_json::json!({"action": "frobnicate"}),
            ctx: ctx_with_mode(PermissionMode::Plan),
        };
        let d = h.handle(&ev).await.unwrap();
        assert!(matches!(d, HookDecision::Deny { .. }));
    }

    /// 缺失 `action` 字段在 Plan mode 下同样 deny(防御性)。
    #[tokio::test]
    async fn plan_mode_denies_ast_without_action() {
        let h = PlanModeGate::default_mode();
        let ev = HookEvent::PreToolUse {
            tool: "ast".into(),
            args: serde_json::json!({}),
            ctx: ctx_with_mode(PermissionMode::Plan),
        };
        let d = h.handle(&ev).await.unwrap();
        assert!(matches!(d, HookDecision::Deny { .. }));
    }

    /// 非 Plan mode(Auto / Prompt / Deny)下 `ast` 所有 action 一律 Allow
    /// —— PlanModeGate 只在 Plan mode 生效,不能污染普通执行模式。
    #[tokio::test]
    async fn non_plan_mode_allows_all_ast_actions() {
        let h = PlanModeGate::default_mode();
        for mode in [
            PermissionMode::Auto,
            PermissionMode::Prompt,
            PermissionMode::Deny,
        ] {
            for action in ["list_languages", "search", "replace", "rename_symbol"] {
                let ev = HookEvent::PreToolUse {
                    tool: "ast".into(),
                    args: serde_json::json!({"action": action}),
                    ctx: ctx_with_mode(mode),
                };
                assert_eq!(
                    h.handle(&ev).await.unwrap(),
                    HookDecision::Allow,
                    "{mode:?} 不应触发 gate(action={action})"
                );
            }
        }
    }

    /// Plan mode 下 write 到 `.reflect/plan/` 路径放行（plan file 机制）。
    #[tokio::test]
    async fn plan_mode_allows_write_to_plan_dir() {
        let h = PlanModeGate::default_mode();
        let ev = HookEvent::PreToolUse {
            tool: "write".into(),
            args: serde_json::json!({"path": "/workspace/.reflect/plan/my-plan.md"}),
            ctx: ctx_with_mode(PermissionMode::Plan),
        };
        assert_eq!(
            h.handle(&ev).await.unwrap(),
            HookDecision::Allow,
            "write 到 .reflect/plan/ 应在 Plan mode 放行"
        );
    }

    /// Plan mode 下 write 到其他路径仍被 deny（只读语义不变）。
    #[tokio::test]
    async fn plan_mode_denies_write_outside_plan_dir() {
        let h = PlanModeGate::default_mode();
        let ev = HookEvent::PreToolUse {
            tool: "write".into(),
            args: serde_json::json!({"path": "/workspace/src/main.rs"}),
            ctx: ctx_with_mode(PermissionMode::Plan),
        };
        let d = h.handle(&ev).await.unwrap();
        match d {
            HookDecision::Deny { reason } => {
                assert!(
                    reason.contains("write"),
                    "deny reason 应包含工具名: {reason}"
                );
            }
            other => panic!("write 到非 plan 路径应被 deny, 得到 {other:?}"),
        }
    }

    /// Plan mode 下 edit 到 `.reflect/plan/` 路径放行。
    #[tokio::test]
    async fn plan_mode_allows_edit_to_plan_dir() {
        let h = PlanModeGate::default_mode();
        let ev = HookEvent::PreToolUse {
            tool: "edit".into(),
            args: serde_json::json!({"path": "/workspace/.reflect/plan/plan-v2.md"}),
            ctx: ctx_with_mode(PermissionMode::Plan),
        };
        assert_eq!(
            h.handle(&ev).await.unwrap(),
            HookDecision::Allow,
            "edit 到 .reflect/plan/ 应在 Plan mode 放行"
        );
    }
}
