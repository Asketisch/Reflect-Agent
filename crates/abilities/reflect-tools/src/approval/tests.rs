//! `approval` 模块单测(从原 `approval.rs` 内联 `#[cfg(test)] mod tests`
//! 迁移而来,逻辑无任何改动)。

use super::*;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::timeout;

use reflect_protocol::{
    ApprovalKind, Event, EventMsg, PermissionMode, ReviewDecision, RiskLevel, ToolError,
};

fn make_gate() -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    (ApprovalGate::new(tx, "sub-1"), rx)
}

#[tokio::test]
async fn ask_tool_emits_event_and_waits_for_decision() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();

    // 启动 ask;它会阻塞在 oneshot 上。
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move {
        g.ask_tool(
            "bash",
            &serde_json::json!({"cmd": "ls"}),
            RiskLevel::Medium,
            &cancel_c,
        )
        .await
    });

    // 读取 emit 的 event 并提取 request_id。
    let ev = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::ApprovalRequest(req) => {
            assert!(matches!(req.kind, ApprovalKind::Tool { .. }));
            assert_eq!(req.risk, RiskLevel::Medium);
            req.request_id
        }
        other => panic!("expected ApprovalRequest, got {other:?}"),
    };

    // 完成这次 approval。
    assert!(gate_arc.complete(&request_id, ReviewDecision::Approve));

    let decision = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decision, ReviewDecision::Approve);
}

#[tokio::test]
async fn cancel_short_circuits_to_deny() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();

    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move {
        g.ask_tool("bash", &serde_json::json!({}), RiskLevel::Low, &cancel_c)
            .await
    });

    // 给 asker 一点时间完成注册。
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let decision = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(decision, ReviewDecision::Deny { .. }));
    // Waiter 应已被移除。
    assert!(gate_arc.waiters().lock().is_empty());
}

#[tokio::test]
async fn complete_unknown_request_returns_false() {
    let (gate, _rx) = make_gate();
    assert!(!gate.complete("nope", ReviewDecision::Approve));
}

#[tokio::test]
async fn closed_event_channel_auto_denies() {
    let (tx, rx) = mpsc::channel::<Event>(8);
    drop(rx);
    let gate = ApprovalGate::new(tx, "sub-x");
    let cancel = CancellationToken::new();
    let d = gate
        .ask_tool("bash", &serde_json::json!({}), RiskLevel::Low, &cancel)
        .await;
    assert!(matches!(d, ReviewDecision::Deny { .. }));
}

#[tokio::test]
async fn session_allow_persists() {
    let (gate, _rx) = make_gate();
    assert!(!gate.is_session_allowed("bash"));
    gate.allow_for_session("bash");
    assert!(gate.is_session_allowed("bash"));
}

// ── S5a:permission_resolver 短路测试 ────────────────────────────────
//
// 验证 ask_tool 入口查 resolver:
// - Allow → 直接 Approve(不 emit ApprovalRequest event,不调 modal)。
// - Deny  → 直接 Deny { reason }。
// - NoMatch → 走原 modal 流程(emit event 等用户决策)。
// - None resolver → 同 NoMatch(向后兼容)。
//
// 用 `reflect_permissions::InMemoryPermissionStore` + `StorePermissionResolver`
// 构造真实 store 链;测试短路路径需要 `CancellationToken` 不被 cancel。

use reflect_permissions::{
    InMemoryPermissionStore, PermissionAction, PermissionResolver, PermissionRule,
    StorePermissionResolver,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 构造挂载 resolver 的 gate(共享 in-memory store)。
fn make_gate_with_resolver(
    rules: Vec<(&str, PermissionAction)>,
) -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let store: Arc<dyn reflect_permissions::PermissionStore> =
        Arc::new(InMemoryPermissionStore::new());
    // sync 闭包注入 rules;InMemoryPermissionStore::add 是 async,需
    // 走 block_on 把 rules 灌进 store。但 tests 已经是 #[tokio::test],
    // 用 spawn_blocking 不便 —— 直接 futures::executor::block_on。
    for (tool, action) in rules {
        let rule = PermissionRule {
            tool: tool.into(),
            action,
            tool_glob: None,
            shell_pattern: None,
        };
        futures::executor::block_on(store.add(rule)).unwrap();
    }
    let resolver: Arc<dyn PermissionResolver> = Arc::new(StorePermissionResolver::new(store));
    let waiters: ApprovalWaiters = Arc::new(Mutex::new(HashMap::new()));
    let session_allow: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    (
        ApprovalGate::with_state(
            tx,
            "sub-r",
            waiters,
            session_allow,
            Some(resolver),
            None,
            None,
            None,
        ),
        rx,
    )
}

/// `Allow` rule 短路 → 直接 Approve,**不**emit ApprovalRequest event
///(否则下游 `rx` 收到 event 但我们期望 Approve 是终态)。
#[tokio::test]
async fn ask_tool_allow_rule_short_circuits_to_approve() {
    let (gate, mut rx) = make_gate_with_resolver(vec![("Bash", PermissionAction::Allow)]);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool("Bash", &serde_json::json!({}), RiskLevel::Low, &cancel)
        .await;
    assert_eq!(decision, ReviewDecision::Approve);
    // rx 应该空(没 emit event),因为短路直接返回。
    assert!(
        rx.try_recv().is_err(),
        "should not emit event on Allow short-circuit"
    );
}

/// `Deny` rule 短路 → 直接 Deny { reason },**不**emit event。
#[tokio::test]
async fn ask_tool_deny_rule_short_circuits_to_deny() {
    let (gate, mut rx) = make_gate_with_resolver(vec![("Write", PermissionAction::Deny)]);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool("Write", &serde_json::json!({}), RiskLevel::Medium, &cancel)
        .await;
    match decision {
        ReviewDecision::Deny { reason } => {
            assert!(
                reason.contains("Write"),
                "reason should mention tool: {reason}"
            );
            assert!(reason.contains("denied by rule"), "got: {reason}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "should not emit event on Deny short-circuit"
    );
}

/// `NoMatch`(resolver 存在但 tool 无规则)→ 走原 modal 流程,emit event。
/// 验证方法:cancel token,期望 ask 返回 Deny { cancelled }。
#[tokio::test]
async fn ask_tool_no_match_falls_through_to_modal() {
    let (gate, _rx) = make_gate_with_resolver(vec![("Bash", PermissionAction::Allow)]);
    // 查询 "Write" → store 里没规则 → NoMatch → fall through 到 modal。
    let cancel = CancellationToken::new();
    let cancel_for_spawn = cancel.clone();
    let ask = tokio::spawn(async move {
        gate.ask_tool(
            "Write",
            &serde_json::json!({}),
            RiskLevel::Medium,
            &cancel_for_spawn,
        )
        .await
    });
    // 短暂等让 ask 跑进 modal 等待。
    tokio::time::sleep(Duration::from_millis(20)).await;
    // cancel token 让 modal 短路返回 Deny { cancelled }。
    cancel.cancel();
    let decision = ask.await.unwrap();
    assert!(matches!(decision, ReviewDecision::Deny { .. }));
}

/// `None` resolver(向后兼容路径)→ 走原 modal 流程。
#[tokio::test]
async fn ask_tool_no_resolver_falls_through_to_modal() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    let cancel_for_spawn = cancel.clone();
    let ask = tokio::spawn(async move {
        gate.ask_tool(
            "Bash",
            &serde_json::json!({}),
            RiskLevel::Low,
            &cancel_for_spawn,
        )
        .await
    });
    // 短暂等让 ask 跑进 modal 等待。
    tokio::time::sleep(Duration::from_millis(20)).await;
    // cancel 让 ask 返回。
    cancel.cancel();
    let decision = ask.await.unwrap();
    assert!(matches!(decision, ReviewDecision::Deny { .. }));
}

// ── v1.1.0 P1 #14:ask_question 测试 ─────────────────────────────────
//
// 覆盖:
// 1. happy path:emit event + 等回执 + 拿回答案。
// 2. cancel:等待被取消 → ToolError::Cancelled。
// 3. event channel 关闭 → ToolError::Execution。
// 4. validation:空 questions / 超过 4 道 / options 越界 / header 超长。
// 5. complete_question 找不到 waiter → false。
// 6. multi-question flow:3 题 / multi_select / Other 自定义。

use reflect_protocol::question::{Answer, AskUserAnswer, Question, QuestionOption};

fn sample_questions() -> Vec<Question> {
    vec![
        Question {
            header: "Lang".into(),
            question: "Pick a language".into(),
            options: vec![
                QuestionOption {
                    label: "Rust".into(),
                    description: "safe + fast".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "Go".into(),
                    description: "simple".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        },
        Question {
            header: "Deploy".into(),
            question: "Where to deploy?".into(),
            options: vec![
                QuestionOption {
                    label: "AWS".into(),
                    description: "managed".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "GCP".into(),
                    description: "managed".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "Self".into(),
                    description: "BYO infra".into(),
                    preview: None,
                },
            ],
            multi_select: true,
        },
    ]
}

#[tokio::test]
async fn ask_question_emits_event_and_waits_for_response() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);

    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move { g.ask_question(sample_questions(), &cancel_c).await });

    // 取出 emit 的 event,验证 AskUserQuestion + request_id 是 uuid。
    let ev = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserQuestion(e) => {
            assert_eq!(e.questions.len(), 2);
            assert!(!e.questions[0].multi_select);
            assert!(e.questions[1].multi_select);
            e.request_id
        }
        other => panic!("expected AskUserQuestion, got {other:?}"),
    };
    // uuid 格式:8-4-4-4-12。
    assert_eq!(request_id.len(), 36, "got: {request_id}");

    // 回执:用户答了 2 道题(其中第 1 道用 "Other" 自定义文本)。
    let answers = AskUserAnswer {
        answers: vec![
            Answer::single(0).with_custom("prefer async runtime"),
            Answer::multi(vec![0, 2]),
        ],
    };
    assert!(gate_arc.complete_question(&request_id, answers.clone()));

    let result = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.answers.len(), 2);
    assert_eq!(result.answers[0].selected, vec![0]);
    assert_eq!(
        result.answers[0].custom.as_deref(),
        Some("prefer async runtime")
    );
    assert_eq!(result.answers[1].selected, vec![0, 2]);
}

#[tokio::test]
async fn ask_question_cancelled_returns_cancelled_error() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move { g.ask_question(sample_questions(), &cancel_c).await });

    // 让 ask 跑进 modal 等待。
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let err = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
    // waiter 已被清理。
    assert!(gate_arc.question_waiters().lock().is_empty());
}

#[tokio::test]
async fn ask_question_closed_event_channel_returns_execution_error() {
    let (tx, rx) = mpsc::channel::<Event>(8);
    drop(rx);
    let gate = ApprovalGate::new(tx, "sub-x");
    let cancel = CancellationToken::new();
    let err = gate
        .ask_question(sample_questions(), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
}

#[tokio::test]
async fn ask_question_rejects_empty_questions() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    let err = gate.ask_question(vec![], &cancel).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn ask_question_rejects_too_many_questions() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    let five_questions: Vec<Question> = (0..5)
        .map(|i| Question {
            header: format!("Q{i}"),
            question: format!("question {i}"),
            options: vec![
                QuestionOption {
                    label: "A".into(),
                    description: "a".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "B".into(),
                    description: "b".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        })
        .collect();
    let err = gate
        .ask_question(five_questions, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn ask_question_rejects_invalid_options_count() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    // 1 个 option — 越下界。
    let bad = vec![Question {
        header: "H".into(),
        question: "Q".into(),
        options: vec![QuestionOption {
            label: "A".into(),
            description: "a".into(),
            preview: None,
        }],
        multi_select: false,
    }];
    let err = gate.ask_question(bad, &cancel).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn ask_question_rejects_header_too_long() {
    let (gate, _rx) = make_gate();
    let cancel = CancellationToken::new();
    let long = "a".repeat(20);
    let bad = vec![Question {
        header: long,
        question: "Q".into(),
        options: vec![
            QuestionOption {
                label: "A".into(),
                description: "a".into(),
                preview: None,
            },
            QuestionOption {
                label: "B".into(),
                description: "b".into(),
                preview: None,
            },
        ],
        multi_select: false,
    }];
    let err = gate.ask_question(bad, &cancel).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs { .. }));
}

#[tokio::test]
async fn complete_question_unknown_request_returns_false() {
    let (gate, _rx) = make_gate();
    assert!(!gate.complete_question("nope", AskUserAnswer::empty(1)));
}

#[tokio::test]
async fn ask_question_user_pressed_esc_yields_empty_answers() {
    // 用户在 TUI modal 按 Esc → submission_loop 收到 Op::AskUserQuestionResponse
    // 带空 `AskUserAnswer` → `gate.complete_question` 路由回 waiter。
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move { g.ask_question(sample_questions(), &cancel_c).await });

    let ev = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserQuestion(e) => e.request_id,
        other => panic!("expected AskUserQuestion, got {other:?}"),
    };

    // 模拟用户按 Esc:回执空答案(answers.len() == questions.len())。
    let empty = AskUserAnswer::empty(2);
    assert!(gate_arc.complete_question(&request_id, empty));

    let result = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.answers.len(), 2);
    for ans in &result.answers {
        assert!(ans.selected.is_empty());
        assert!(ans.custom.is_none());
    }
}

fn make_gate_with_permission_mode(mode: PermissionMode) -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let pm = Arc::new(parking_lot::RwLock::new(mode));
    (
        ApprovalGate::with_state(
            tx,
            "sub-pm",
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            None,
            None,
            None,
            Some(pm),
        ),
        rx,
    )
}

#[tokio::test]
async fn accept_edits_short_circuits_edit_tools_only() {
    let (gate, mut rx) = make_gate_with_permission_mode(PermissionMode::AcceptEdits);
    let cancel = CancellationToken::new();
    let edit = gate
        .ask_tool(
            "write",
            &serde_json::json!({"path": "a"}),
            RiskLevel::Medium,
            &cancel,
        )
        .await;
    assert_eq!(edit, ReviewDecision::Approve);
    assert!(rx.try_recv().is_err());

    let bash = tokio::spawn({
        let gate = gate;
        let cancel = cancel.clone();
        async move {
            gate.ask_tool("bash", &serde_json::json!({}), RiskLevel::Medium, &cancel)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    assert!(matches!(bash.await.unwrap(), ReviewDecision::Deny { .. }));
}

#[tokio::test]
async fn bubble_mode_emits_event_and_auto_approves() {
    let (gate, mut rx) = make_gate_with_permission_mode(PermissionMode::Bubble);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool("bash", &serde_json::json!({}), RiskLevel::Low, &cancel)
        .await;
    assert_eq!(decision, ReviewDecision::Approve);
    let ev = rx.recv().await.unwrap();
    assert!(matches!(ev.msg, EventMsg::PermissionBubble(_)));
}

#[tokio::test]
async fn ask_user_emits_event_and_returns_text() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let asker = tokio::spawn(async move { g.ask_user("ask_user", "Your name?", &cancel, 0).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(e) => {
            assert_eq!(e.prompt, "Your name?");
            e.request_id
        }
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    assert!(complete_ask_user_input(
        gate_arc.user_input_waiters(),
        &request_id,
        "Alice".into()
    ));

    let text = asker.await.unwrap().unwrap();
    assert_eq!(text, "Alice");
}

#[tokio::test]
async fn ask_user_opts_emits_secret_and_placeholder() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let asker = tokio::spawn(async move {
        g.ask_user_opts(
            "ask_user",
            "Enter API key",
            &cancel,
            0,
            true,
            Some("sk-..."),
        )
        .await
    });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(e) => {
            assert_eq!(e.prompt, "Enter API key");
            assert_eq!(e.secret, Some(true));
            assert_eq!(e.placeholder.as_deref(), Some("sk-..."));
            e.request_id
        }
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    assert!(complete_ask_user_input(
        gate_arc.user_input_waiters(),
        &request_id,
        "secret-value".into()
    ));
    assert_eq!(asker.await.unwrap().unwrap(), "secret-value");
}

// ── v1.2 review P1:bug-1:ask_user timeout 路径 ──────────────────────
//
// 覆盖:
// 1. timeout_secs = 0:永不超时,cancel token 才能终结等待。
// 2. timeout_secs > 0:超时到达返回 ToolError::Execution,waiter 被清理
//    (后续 complete 同 id 返回 false)。
// 3. timeout_secs > 0:在超时到达前用户响应 → 正常返回,优先级高于 timeout。

#[tokio::test]
async fn ask_user_timeout_zero_waits_forever_until_cancel() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_for_ask = cancel.clone();
    let asker = tokio::spawn(async move {
        g.ask_user("ask_user", "Long running?", &cancel_for_ask, 0)
            .await
    });

    // 拿出 emit 的 event 表明 ask 已就位。
    let ev = rx.recv().await.unwrap();
    let _ = match ev.msg {
        EventMsg::AskUserInput(e) => e.request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    // 等 200ms 确认 ask 没有 timeout 自杀。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !asker.is_finished(),
        "ask_user(0) should not self-terminate"
    );

    // cancel 终结。
    cancel.cancel();
    let err = asker.await.unwrap().unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
    assert!(gate_arc.user_input_waiters().lock().is_empty());
}

#[tokio::test]
async fn ask_user_timeout_fires_execution_error_and_clears_waiter() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let asker = tokio::spawn(async move { g.ask_user("ask_user", "Soon?", &cancel, 1).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(e) => e.request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    // 等超过 1s 让 timeout 兜底触发。
    let err = tokio::time::timeout(Duration::from_secs(3), asker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    match err {
        ToolError::Execution(msg) => {
            assert!(msg.contains("timed out"), "got: {msg}");
            assert!(msg.contains("1"), "got: {msg}");
        }
        other => panic!("expected Execution timeout error, got: {other:?}"),
    }
    // waiter 已被清理,二次 complete 返回 false。
    assert!(gate_arc.user_input_waiters().lock().is_empty());
    assert!(!complete_ask_user_input(
        gate_arc.user_input_waiters(),
        &request_id,
        "late".into()
    ));
}

#[tokio::test]
async fn ask_user_response_wins_over_timeout() {
    let (gate, mut rx) = make_gate();
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let asker = tokio::spawn(async move { g.ask_user("ask_user", "Race?", &cancel, 5).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(e) => e.request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    // 50ms 内响应,远早于 5s timeout。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(complete_ask_user_input(
        gate_arc.user_input_waiters(),
        &request_id,
        "fast".into()
    ));

    let text = tokio::time::timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(text, "fast");
}

// ── v1.2 review P1:bug-2:ask_user 权限短路 ──────────────────────────
//
// 覆盖:
// 1. resolver Deny rule → InvalidArgs,不发 event。
// 2. resolver Allow rule → 不短路(自由文本无默认值),正常发 event。
// 3. session PermissionMode::Deny → InvalidArgs。
// 4. session PermissionMode::Bubble → InvalidArgs(无法自动回答)。
// 5. resolver NoMatch + 无 mode → 走原 modal 流程。

/// 构造挂载 resolver 的 gate(共享 in-memory store)。
fn make_ask_user_gate_with_resolver(
    rules: Vec<(&str, PermissionAction)>,
) -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let store: Arc<dyn reflect_permissions::PermissionStore> =
        Arc::new(InMemoryPermissionStore::new());
    for (tool, action) in rules {
        let rule = PermissionRule {
            tool: tool.into(),
            action,
            tool_glob: None,
            shell_pattern: None,
        };
        futures::executor::block_on(store.add(rule)).unwrap();
    }
    let resolver: Arc<dyn PermissionResolver> = Arc::new(StorePermissionResolver::new(store));
    (
        ApprovalGate::with_state(
            tx,
            "sub-au",
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Some(resolver),
            None,
            None,
            None,
        ),
        rx,
    )
}

/// 构造挂载 session permission mode 的 gate(无 resolver)。
fn make_ask_user_gate_with_mode(mode: PermissionMode) -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let pm = Arc::new(parking_lot::RwLock::new(mode));
    (
        ApprovalGate::with_state(
            tx,
            "sub-aum",
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            None,
            None,
            None,
            Some(pm),
        ),
        rx,
    )
}

#[tokio::test]
async fn ask_user_resolver_deny_short_circuits_to_invalid_args() {
    let (gate, mut rx) =
        make_ask_user_gate_with_resolver(vec![("ask_user", PermissionAction::Deny)]);
    let cancel = CancellationToken::new();
    let err = gate
        .ask_user("ask_user", "Q?", &cancel, 0)
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("denied by rule"), "got: {message}");
            assert!(message.contains("ask_user"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
    // 不发 event。
    assert!(rx.try_recv().is_err(), "should not emit event on Deny");
}

#[tokio::test]
async fn ask_user_resolver_allow_does_not_short_circuit() {
    // 契约:`Allow` rule 不短路(自由文本无默认值)。验证方法:发 event,
    // cancel token 终结等待,确认走到了 modal 流程。
    let (gate, _rx) = make_ask_user_gate_with_resolver(vec![("ask_user", PermissionAction::Allow)]);
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let asker = tokio::spawn(async move { g.ask_user("ask_user", "Q?", &cancel_c, 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    let err = asker.await.unwrap().unwrap_err();
    // 走 modal 流程 → cancel 终结 → Cancelled,不是 InvalidArgs。
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
}

#[tokio::test]
async fn ask_user_session_mode_deny_short_circuits() {
    let (gate, mut rx) = make_ask_user_gate_with_mode(PermissionMode::Deny);
    let cancel = CancellationToken::new();
    let err = gate
        .ask_user("ask_user", "Q?", &cancel, 0)
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("deny"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn ask_user_session_mode_bubble_short_circuits() {
    // Bubble 模式无法对自由文本做"自动同意"——保守退化为 Deny。
    let (gate, mut rx) = make_ask_user_gate_with_mode(PermissionMode::Bubble);
    let cancel = CancellationToken::new();
    let err = gate
        .ask_user("ask_user", "Q?", &cancel, 0)
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("bubble"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
    assert!(rx.try_recv().is_err());
}

// ── P2 yolo-classifier 接入测试 ─────────────────────────────────────
//
// Auto 模式 + resolver 无规则 + mode 未自动批准 → 查 yolo 分类器:
// - Allow(高置信 ≥ 阈值) → 自动批准,不 emit event。
// - Deny → 拒绝,不 emit event。
// - Ask / Allow 低置信 → 落 modal(emit event + cancel 终结)。
// - 无分类器 → 维持原 modal 行为。

/// 构造无 resolver / Auto 模式 + 挂 yolo 分类器的 gate。
fn make_gate_with_yolo(
    classifier: Arc<dyn reflect_permissions::YoloClassifier>,
    threshold: f32,
) -> (ApprovalGate, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let pm = Arc::new(parking_lot::RwLock::new(PermissionMode::Auto));
    let g = ApprovalGate::with_state(
        tx,
        "sub-yolo",
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashSet::new())),
        None,
        None,
        None,
        Some(pm),
    );
    g.set_yolo_classifier(Some(classifier), Some(threshold));
    (g, rx)
}

/// 用真实 `HeuristicYoloClassifier`:只读工具(Read)→ Allow(0.85),
/// 默认阈值 0.8 → 自动批准。
#[tokio::test]
async fn yolo_classifier_auto_approves_readonly_in_auto_mode() {
    let (gate, mut rx) =
        make_gate_with_yolo(Arc::new(reflect_permissions::HeuristicYoloClassifier), 0.8);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool("Read", &serde_json::json!({}), RiskLevel::Low, &cancel)
        .await;
    assert_eq!(decision, ReviewDecision::Approve);
    assert!(rx.try_recv().is_err(), "高置信 Allow 不应 emit event");
}

/// 启发式对 Write 默认 Ask(置信 0.5 < 阈值 0.8)→ 落 modal。
#[tokio::test]
async fn yolo_classifier_low_confidence_falls_through_to_modal() {
    let (gate, _rx) =
        make_gate_with_yolo(Arc::new(reflect_permissions::HeuristicYoloClassifier), 0.8);
    let cancel = CancellationToken::new();
    let gate_arc = Arc::new(gate);
    let g = gate_arc.clone();
    let cancel_c = cancel.clone();
    let ask = tokio::spawn(async move {
        g.ask_tool(
            "Write",
            &serde_json::json!({}),
            RiskLevel::Medium,
            &cancel_c,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    let decision = ask.await.unwrap();
    assert!(
        matches!(decision, ReviewDecision::Deny { .. }),
        "低置信 Ask 走 modal,被 cancel 终结为 Deny"
    );
}

/// 自定义 Deny 分类器 → 直接拒绝。
#[tokio::test]
async fn yolo_classifier_deny_short_circuits_to_deny() {
    struct DenyAll;
    impl reflect_permissions::YoloClassifier for DenyAll {
        fn classify(&self, _tool: &str, _args: &str) -> reflect_permissions::YoloClassification {
            reflect_permissions::YoloClassification {
                suggestion: reflect_permissions::YoloSuggestion::Deny,
                confidence: 0.99,
                reason: "deny all".into(),
            }
        }
    }
    let (gate, mut rx) = make_gate_with_yolo(Arc::new(DenyAll), 0.8);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool("Read", &serde_json::json!({}), RiskLevel::Low, &cancel)
        .await;
    match decision {
        ReviewDecision::Deny { reason } => assert!(reason.contains("yolo classifier")),
        other => panic!("expected Deny, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "Deny 不应 emit event");
}

/// v1.3 safety baseline(plan §五):`Bubble` 会话通常对每个 Prompt-mode
/// 工具自动放行。新的高危 gate 仍必须为携带 `RiskLevel::High` 的工具
/// 弹出每次都需审批的 modal。本测试用真实的
/// `session_permission_mode = Bubble` 装配 gate,发送一条 `High`-risk
/// 调用,并断言 gate **发出** `ApprovalRequest`(而不是默默返回
/// `Approve`)。
#[tokio::test]
async fn bubble_mode_does_not_auto_approve_high_risk_tool() {
    use reflect_protocol::EventMsg;
    let (tx, mut rx) = mpsc::channel::<Event>(4);
    let waiters: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ReviewDecision>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let session_allow: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mode_lock: Arc<parking_lot::RwLock<PermissionMode>> =
        Arc::new(parking_lot::RwLock::new(PermissionMode::Bubble));
    let gate = ApprovalGate::with_state(
        tx,
        "sub-bubble",
        waiters.clone(),
        session_allow,
        None,
        None,
        None,
        Some(mode_lock),
    );
    let gate = Arc::new(gate);
    let cancel = CancellationToken::new();
    let gate_c = gate.clone();
    let asker = tokio::spawn(async move {
        gate_c
            .ask_tool(
                "bash",
                &serde_json::json!({"cmd": "sudo apt update"}),
                RiskLevel::High,
                &cancel,
            )
            .await
    });
    // Bubble + High 仍然必须触发审批请求,不能自动放行。
    let ev = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("ApprovalRequest must arrive even in Bubble mode for High risk")
        .expect("event channel open");
    let req_id = match ev.msg {
        EventMsg::ApprovalRequest(req) => {
            assert_eq!(req.risk, RiskLevel::High);
            req.request_id
        }
        other => panic!("expected ApprovalRequest, got {other:?}"),
    };
    assert!(
        gate.complete(&req_id, ReviewDecision::Approve),
        "waiter must be present"
    );
    let decision = timeout(Duration::from_secs(2), asker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(decision, ReviewDecision::Approve));
}

/// 上一测试的伴随测试:`Bubble` 模式**确实**对 `Medium`-risk 工具自动
/// 放行(保留对低影响编辑/搜索的旧行为)。决策与 High-risk 路径的唯一
/// 区别在于 `risk` 参数;此处的回归断言将该行为锁定下来。
#[tokio::test]
async fn bubble_mode_still_auto_approves_medium_risk_tool() {
    let (tx, mut rx) = mpsc::channel::<Event>(4);
    let waiters: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ReviewDecision>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let session_allow: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mode_lock: Arc<parking_lot::RwLock<PermissionMode>> =
        Arc::new(parking_lot::RwLock::new(PermissionMode::Bubble));
    let gate = ApprovalGate::with_state(
        tx,
        "sub-bubble-med",
        waiters.clone(),
        session_allow,
        None,
        None,
        None,
        Some(mode_lock),
    );
    let gate = Arc::new(gate);
    let cancel = CancellationToken::new();
    let decision = gate
        .ask_tool(
            "grep",
            &serde_json::json!({"pattern": "TODO"}),
            RiskLevel::Medium,
            &cancel,
        )
        .await;
    assert!(
        matches!(decision, ReviewDecision::Approve),
        "Bubble must auto-approve Medium-risk: got {decision:?}"
    );
    // Bubble 即使自动放行也会出于审计目的发 `PermissionBubble` 事件;
    // 检查 channel 已收到该事件。
    let ev = timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("bubble event must arrive")
        .expect("bubble event body");
    assert!(matches!(
        ev.msg,
        EventMsg::PermissionBubble(b) if b.tool_name == "grep"
    ));
}
