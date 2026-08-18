//! `HookEngine::merge` 的集成测试(完整决策矩阵)。

use reflect_hooks::{HookDecision, HookEngine, SystemMessage};
use reflect_protocol::PermissionMode;

#[test]
fn allow_allow_is_allow() {
    assert_eq!(
        HookEngine::merge(vec![HookDecision::Allow, HookDecision::Allow]),
        HookDecision::Allow
    );
}

#[test]
fn allow_deny_is_deny() {
    let merged = HookEngine::merge(vec![
        HookDecision::Allow,
        HookDecision::Deny { reason: "x".into() },
    ]);
    match merged {
        HookDecision::Deny { reason } => assert_eq!(reason, "x"),
        other => panic!("expected deny, got {other:?}"),
    }
}

#[test]
fn modify_args_takes_last() {
    let merged = HookEngine::merge(vec![
        HookDecision::ModifyArgs(serde_json::json!({"a": 1})),
        HookDecision::ModifyArgs(serde_json::json!({"b": 2})),
    ]);
    match merged {
        HookDecision::ModifyArgs(v) => assert_eq!(v, serde_json::json!({"b": 2})),
        other => panic!("expected modify, got {other:?}"),
    }
}

#[test]
fn inject_message_combines_in_combined() {
    let merged = HookEngine::merge(vec![
        HookDecision::InjectMessage(SystemMessage::new("a")),
        HookDecision::InjectMessage(SystemMessage::new("b")),
    ]);
    match merged {
        HookDecision::Combined(parts) => {
            assert_eq!(parts.len(), 2);
            match &parts[0] {
                HookDecision::InjectMessage(m) => assert_eq!(m.content, "a"),
                _ => panic!(),
            }
            match &parts[1] {
                HookDecision::InjectMessage(m) => assert_eq!(m.content, "b"),
                _ => panic!(),
            }
        }
        other => panic!("expected combined, got {other:?}"),
    }
}

#[test]
fn deny_with_modify_keeps_both_via_combined() {
    let merged = HookEngine::merge(vec![
        HookDecision::ModifyArgs(serde_json::json!({"x": 1})),
        HookDecision::Deny {
            reason: "blocked".into(),
        },
    ]);
    match merged {
        HookDecision::Combined(parts) => {
            assert_eq!(parts.len(), 2);
            assert!(matches!(parts[0], HookDecision::Deny { .. }));
            assert!(matches!(parts[1], HookDecision::ModifyArgs(_)));
        }
        other => panic!("expected combined, got {other:?}"),
    }
}

#[test]
fn permission_override_does_not_collide_with_modify() {
    let merged = HookEngine::merge(vec![
        HookDecision::PermissionOverride(PermissionMode::Deny),
        HookDecision::ModifyArgs(serde_json::json!({"x": 1})),
    ]);
    // M3 v0:PermissionOverride 会被收集但不出现在合并结果里,
    // 只回 ModifyArgs。调用方需要从各 hook 决策里另行查看。
    match merged {
        HookDecision::ModifyArgs(v) => assert_eq!(v, serde_json::json!({"x": 1})),
        other => panic!("expected modify, got {other:?}"),
    }
}

#[test]
fn combined_input_is_flattened() {
    let merged = HookEngine::merge(vec![HookDecision::Combined(vec![
        HookDecision::Allow,
        HookDecision::Deny {
            reason: "from_combined".into(),
        },
    ])]);
    match merged {
        HookDecision::Deny { reason } => assert_eq!(reason, "from_combined"),
        other => panic!("expected deny, got {other:?}"),
    }
}
