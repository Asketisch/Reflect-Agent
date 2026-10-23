//! v1.5 R3 — ShellHook × HookEngine 集成:外部命令 hook 注册进引擎后,
//! dispatch 真正执行命令并按 stdout 决策裁决(拒绝 / 放行)。

use std::time::Duration;

use reflect_hooks::{DEFAULT_SHELL_HOOK_TIMEOUT, HookEngine, HookEvent, HookEventKind, ShellHook};

fn pre_tool_event(tool: &str) -> HookEvent {
    serde_json::from_value(serde_json::json!({
        "kind": "pre_tool_use",
        "tool": tool,
        "args": {"cmd": "echo hi"},
        "ctx": {
            "session_id": "00000000-0000-0000-0000-000000000000",
            "turn_id": "00000000-0000-0000-0000-000000000000",
            "workspace": "/tmp",
            "permission_mode": "auto"
        }
    }))
    .unwrap()
}

/// deny 型 shell hook:引擎 dispatch 返回拒绝裁决(工具队列将不执行)。
#[tokio::test]
async fn engine_dispatch_shell_hook_denies_tool() {
    let engine = HookEngine::new();
    engine.register(ShellHook::new(
        "plugin:sec:PreToolUse#0",
        HookEventKind::PreToolUse,
        Some("bash".to_string()),
        "echo '{\"decision\":\"deny\",\"reason\":\"bash is forbidden by sec plugin\"}'",
        DEFAULT_SHELL_HOOK_TIMEOUT,
    ));

    let decision = engine.dispatch(&pre_tool_event("bash")).await;
    let resolved = decision.resolve();
    assert!(resolved.denied(), "shell hook 应拒绝 bash: {resolved:?}");
    assert_eq!(
        resolved.deny_reason.as_deref(),
        Some("bash is forbidden by sec plugin")
    );
}

/// matcher 不匹配(read 工具)→ 不执行命令,放行。
#[tokio::test]
async fn engine_dispatch_shell_hook_ignores_other_tools() {
    let engine = HookEngine::new();
    engine.register(ShellHook::new(
        "plugin:sec:PreToolUse#0",
        HookEventKind::PreToolUse,
        Some("bash".to_string()),
        "echo '{\"decision\":\"deny\"}'",
        DEFAULT_SHELL_HOOK_TIMEOUT,
    ));

    let decision = engine.dispatch(&pre_tool_event("read")).await;
    assert!(!decision.resolve().denied(), "matcher 不匹配应放行");
}

/// 多 hook 组合:deny hook + allow hook → Deny 优先。
#[tokio::test]
async fn engine_dispatch_deny_wins_over_allow() {
    let engine = HookEngine::new();
    engine.register(ShellHook::new(
        "a",
        HookEventKind::PreToolUse,
        None,
        "cat",
        Duration::from_secs(5),
    ));
    engine.register(ShellHook::new(
        "b",
        HookEventKind::PreToolUse,
        None,
        "echo '{\"decision\":\"deny\",\"reason\":\"veto\"}'",
        Duration::from_secs(5),
    ));

    let decision = engine.dispatch(&pre_tool_event("bash")).await;
    assert!(decision.resolve().denied());
}

/// 前缀注销:unregister_by_prefix 清掉整组插件 hook。
#[tokio::test]
async fn unregister_by_prefix_clears_plugin_scope() {
    let engine = HookEngine::new();
    for name in ["plugin:p1:Stop#0", "plugin:p1:Stop#1", "plugin:p2:Stop#0"] {
        engine.register(ShellHook::new(
            name,
            HookEventKind::Stop,
            None,
            "cat",
            Duration::from_secs(5),
        ));
    }
    assert_eq!(engine.unregister_by_prefix("plugin:p1:"), 2);
    assert_eq!(engine.unregister_by_prefix("plugin:p1:"), 0, "幂等");
    // p2 不受影响。
    assert_eq!(engine.unregister_by_prefix("plugin:p2:"), 1);
}
