//! v1.1.0 P1 #14:`ask_user_question` 端到端测试。
//!
//! 覆盖场景(每场景独立测试,无共享状态):
//! 1. 完整 happy path:tool emit `EventMsg::AskUserQuestion` → 用 `complete_ask_user_question`
//!    全局路由回执答案 → tool 收到 `AskUserAnswer` 并序列化到 `ToolOutput.content`。
//! 2. 多题 multi_select:3 题 / 每题 4 选项 / 含 multi_select / 含 "Other" 自定义文本。
//! 3. 用户取消:回执空 `AskUserAnswer`,tool 收到的 answers 全部是空。
//! 4. 取消短路:tool 在等待中被 cancel token 触发,返回 `ToolError::Cancelled`。
//! 5. 端到端 serde:`EventMsg::AskUserQuestion` + `Op::AskUserQuestionResponse`
//!    在 JSON 上保持配对(`request_id` 一致)。

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reflect_protocol::question::{Answer, AskUserAnswer, Question, QuestionOption};
use reflect_protocol::{AskUserQuestionEvent, ContentBlock, Event, EventMsg, Op, ToolError};
use reflect_tools::approval::ApprovalGate;
use reflect_tools::builtins::AskUserQuestionTool;
use reflect_tools::tool::{Tool, ToolContext};
use reflect_tools::{AskUserQuestionWaiters, complete_ask_user_question};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn sample_question_single() -> Question {
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
    }
}

fn sample_questions_multi() -> Vec<Question> {
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
                    description: "BYO".into(),
                    preview: None,
                },
            ],
            multi_select: true,
        },
    ]
}

fn tool_args(questions: Vec<Question>) -> serde_json::Value {
    let qs: Vec<serde_json::Value> = questions
        .into_iter()
        .map(|q| {
            serde_json::json!({
                "header": q.header,
                "question": q.question,
                "options": q.options.into_iter().map(|o| {
                    let mut m = serde_json::json!({
                        "label": o.label,
                        "description": o.description,
                    });
                    if let Some(p) = o.preview {
                        m["preview"] = serde_json::Value::String(p);
                    }
                    m
                }).collect::<Vec<_>>(),
                "multi_select": q.multi_select,
            })
        })
        .collect();
    serde_json::json!({ "questions": qs })
}

fn make_ctx_with_shared_qw() -> (ToolContext, mpsc::Receiver<Event>, AskUserQuestionWaiters) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let qw: AskUserQuestionWaiters = Arc::new(Mutex::new(Default::default()));
    let gate = Arc::new(ApprovalGate::with_state(
        tx,
        "sub-e2e",
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Default::default())),
        None,
        Some(qw.clone()),
        None,
        None,
    ));
    let ctx = ToolContext {
        approval: Some(gate),
        cancel: CancellationToken::new(),
        ..Default::default()
    };
    (ctx, rx, qw)
}

#[tokio::test]
async fn happy_path_emit_then_complete() {
    // 场景 1:tool 调 execute,emit AskUserQuestion,等回执,序列化结果。
    let tool = AskUserQuestionTool::default();
    let (ctx, mut rx, qw) = make_ctx_with_shared_qw();
    let cancel = ctx.cancel.clone();
    let args = tool_args(vec![sample_question_single()]);
    let runner = tokio::spawn(async move { tool.execute(ctx, args).await });

    // 取出 event,提取 request_id。
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserQuestion(AskUserQuestionEvent {
            request_id,
            questions,
        }) => {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].header, "Lang");
            request_id
        }
        other => panic!("expected AskUserQuestion, got {other:?}"),
    };

    // 全局路由回执。
    let answers = AskUserAnswer {
        answers: vec![Answer::single(0).with_custom("prefer async")],
    };
    assert!(complete_ask_user_question(&qw, &request_id, answers));

    let out = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!out.is_error);
    match &out.content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("answers"));
            assert!(text.contains("prefer async"));
        }
        _ => panic!("expected text"),
    }
    // 二次 complete 失败(no waiter)。
    assert!(!complete_ask_user_question(
        &qw,
        &request_id,
        AskUserAnswer::empty(1)
    ));
    let _ = cancel;
}

#[tokio::test]
async fn multi_question_multi_select_with_other() {
    // 场景 2:3 题 multi-question flow。
    let tool = AskUserQuestionTool::default();
    let (ctx, mut rx, qw) = make_ctx_with_shared_qw();
    let args = tool_args(sample_questions_multi());
    let runner = tokio::spawn(async move { tool.execute(ctx, args).await });

    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserQuestion(AskUserQuestionEvent {
            request_id,
            questions,
        }) => {
            assert_eq!(questions.len(), 2);
            assert!(!questions[0].multi_select);
            assert!(questions[1].multi_select);
            request_id
        }
        other => panic!("expected AskUserQuestion, got {other:?}"),
    };

    // 回执:第 1 题选 1 + Other "TypeScript with Deno",
    //      第 2 题选 0 + 2 (multi)。
    let answers = AskUserAnswer {
        answers: vec![
            Answer::single(1).with_custom("TypeScript with Deno"),
            Answer::multi(vec![0, 2]),
        ],
    };
    assert!(complete_ask_user_question(&qw, &request_id, answers));

    let out = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = match &out.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    // 序列化 JSON 含 "TypeScript with Deno" 和 "selected":[0,2]
    assert!(text.contains("TypeScript with Deno"));
    assert!(text.contains("[0,2]") || text.contains("[0, 2]"));
}

#[tokio::test]
async fn user_cancel_yields_empty_answers() {
    // 场景 3:用户按 Esc → 回执空 answers。
    let tool = AskUserQuestionTool::default();
    let (ctx, mut rx, qw) = make_ctx_with_shared_qw();
    let args = tool_args(sample_questions_multi());
    let runner = tokio::spawn(async move { tool.execute(ctx, args).await });

    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserQuestion(AskUserQuestionEvent { request_id, .. }) => request_id,
        other => panic!("expected AskUserQuestion, got {other:?}"),
    };

    // 模拟 TUI 在 question modal 按 Esc:回执空 answers。
    assert!(complete_ask_user_question(
        &qw,
        &request_id,
        AskUserAnswer::empty(2)
    ));

    let out = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = match &out.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    // 空答案:`selected` 都是 `[]`。
    assert!(text.contains("\"answers\""));
    // 没有 selected 不为空(0 个选项)。
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let arr = parsed["answers"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    for ans in arr {
        let sel = ans["selected"].as_array().unwrap();
        assert!(sel.is_empty(), "cancelled answers must be empty");
        assert!(ans.get("custom").is_none() || ans["custom"].is_null());
    }
}

#[tokio::test]
async fn cancel_token_short_circuits() {
    // 场景 4:cancel 在 wait 期间触发 → ToolError::Cancelled。
    let tool = AskUserQuestionTool::default();
    let (ctx, _rx, _qw) = make_ctx_with_shared_qw();
    let cancel = ctx.cancel.clone();
    let runner = tokio::spawn(async move {
        tool.execute(ctx, tool_args(vec![sample_question_single()]))
            .await
    });

    // 让 tool 跑进 modal 等待。
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let err = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
}

#[tokio::test]
async fn serde_roundtrip_op_response_matches_event() {
    // 场景 5:协议配对 — `Op::AskUserQuestionResponse` 反序列化后
    // 还能在 event_msg 端 match 到对应 `AskUserQuestion`。
    use reflect_protocol::Op as OpEnum;
    let original = OpEnum::AskUserQuestionResponse {
        id: "req-uuid-1".into(),
        answers: AskUserAnswer {
            answers: vec![Answer::single(2).with_custom("PyO3 + maturin")],
        },
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains(r#""type":"ask_user_question_response""#));
    let back: Op = serde_json::from_str(&json).unwrap();
    match back {
        Op::AskUserQuestionResponse { id, answers } => {
            assert_eq!(id, "req-uuid-1");
            assert_eq!(answers.answers[0].selected, vec![2]);
            assert_eq!(answers.answers[0].custom.as_deref(), Some("PyO3 + maturin"));
        }
        other => panic!("wrong variant: {other:?}"),
    }

    // 对应 event 也能反序列化。
    let ev_json = serde_json::json!({
        "type": "ask_user_question",
        "request_id": "req-uuid-1",
        "questions": [
            {
                "header": "Lang",
                "question": "Pick a language",
                "options": [
                    {"label": "Rust", "description": "safe"},
                    {"label": "PyO3", "description": "Python bindings"}
                ],
                "multi_select": false
            }
        ]
    })
    .to_string();
    let ev: EventMsg = serde_json::from_str(&ev_json).unwrap();
    match ev {
        EventMsg::AskUserQuestion(AskUserQuestionEvent {
            request_id,
            questions,
        }) => {
            assert_eq!(request_id, "req-uuid-1");
            assert_eq!(questions.len(), 1);
        }
        other => panic!("wrong event variant: {other:?}"),
    }
}
