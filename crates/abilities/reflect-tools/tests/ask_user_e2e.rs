//! v1.1.0 P1 #15:`ask_user` 端到端测试。
//!
//! v1.2 review 扩充覆盖:
//! - happy / headless(原有 2 个)
//! - cancel / closed-channel / 未知 id / 重复 complete(新 4 个 — P2:bug-6)
//! - 协议 serde roundtrip(新 1 个 — P2:bug-6)
//! - metadata 契约(新 1 个 — P2:bug-6)
//! - 输入校验:empty / whitespace / missing / non-string / extra fields /
//!   oversized prompt(新 6 个 — P1:bug-3 / P2:bug-2 / P2:bug-5)

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reflect_protocol::{AskUserInputEvent, ContentBlock, Event, EventMsg, Op, ToolError};
use reflect_tools::approval::ApprovalGate;
use reflect_tools::builtins::AskUserTool;
use reflect_tools::tool::{Tool, ToolContext};
use reflect_tools::{AskUserInputWaiters, complete_ask_user_input};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 构造挂载共享 `AskUserInputWaiters` 的 gate(e2e helper)。
fn make_ctx_with_shared_uw() -> (ToolContext, mpsc::Receiver<Event>, AskUserInputWaiters) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let uw: AskUserInputWaiters = Arc::new(Mutex::new(Default::default()));
    let gate = Arc::new(ApprovalGate::with_state(
        tx,
        "sub-e2e",
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Default::default())),
        None,
        None,
        Some(uw.clone()),
        None,
    ));
    let ctx = ToolContext {
        approval: Some(gate),
        cancel: CancellationToken::new(),
        ..Default::default()
    };
    (ctx, rx, uw)
}

#[tokio::test]
async fn ask_user_happy_path_returns_user_text() {
    // 场景 1(happy path):tool 调 execute,emit AskUserInput,等回执,返回文本。
    let tool = AskUserTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec = tokio::spawn(async move {
        tool.execute(ctx, serde_json::json!({"prompt": "Continue?"}))
            .await
    });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent {
            request_id, prompt, ..
        }) => {
            assert_eq!(prompt, "Continue?");
            request_id
        }
        other => panic!("expected AskUserInput, got {other:?}"),
    };
    assert!(complete_ask_user_input(&uw, &request_id, "yes".into()));

    let out = exec.await.unwrap().unwrap();
    match &out.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "yes"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_headless_without_gate_returns_execution_error() {
    // 场景 2(headless):ctx.approval = None → ToolError::Execution。
    let tool = AskUserTool;
    let err = tool
        .execute(ToolContext::default(), serde_json::json!({"prompt": "hi"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn ask_user_cancellation_propagates_cancelled_error() {
    // 场景 3:cancel 在 wait 期间触发 → ToolError::Cancelled。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let cancel = ctx.cancel.clone();
    let exec = tokio::spawn(async move {
        tool.execute(ctx, serde_json::json!({"prompt": "long?"}))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(2), exec)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
}

#[tokio::test]
async fn ask_user_closed_event_channel_returns_execution_error() {
    // 场景 4:event channel 已 drop(rx 释放)→ ToolError::Execution。
    let (tx, rx) = mpsc::channel::<Event>(8);
    drop(rx);
    let uw: AskUserInputWaiters = Arc::new(Mutex::new(Default::default()));
    let gate = Arc::new(ApprovalGate::with_state(
        tx,
        "sub-closed",
        Arc::new(Mutex::new(Default::default())),
        Arc::new(Mutex::new(Default::default())),
        None,
        None,
        Some(uw.clone()),
        None,
    ));
    let ctx = ToolContext {
        approval: Some(gate),
        cancel: CancellationToken::new(),
        ..Default::default()
    };
    let tool = AskUserTool;
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
}

#[tokio::test]
async fn ask_user_complete_unknown_id_returns_false() {
    // 场景 5:`complete_ask_user_input` 收到未知 request_id → false。
    let uw: AskUserInputWaiters = Arc::new(Mutex::new(Default::default()));
    let result = complete_ask_user_input(&uw, "nope", "text".into());
    assert!(!result);
}

#[tokio::test]
async fn ask_user_complete_twice_second_fails() {
    // 场景 6:同 id 第一次 complete 成功(用户按 Enter),第二次返回 false
    //(waiter 已被消费)。验证 waiter map 不残留。
    let tool = AskUserTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec =
        tokio::spawn(async move { tool.execute(ctx, serde_json::json!({"prompt": "x"})).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent { request_id, .. }) => request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    // 第一次成功。
    assert!(complete_ask_user_input(&uw, &request_id, "first".into()));
    // 第二次失败(map 已清空)。
    assert!(!complete_ask_user_input(&uw, &request_id, "second".into()));
    // 第一次回执的内容确实到达 tool。
    let out = exec.await.unwrap().unwrap();
    match &out.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "first"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn serde_roundtrip_op_response_matches_event() {
    // 场景 7:协议配对 — `Op::AskUserInputResponse` 反序列化后保留 id
    // + text;对应 `EventMsg::AskUserInput` 也能 roundtrip。
    let original = Op::AskUserInputResponse {
        id: "req-uuid-1".into(),
        text: "yes please".into(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(
        json.contains(r#""type":"ask_user_input_response""#),
        "got: {json}"
    );
    let back: Op = serde_json::from_str(&json).unwrap();
    match back {
        Op::AskUserInputResponse { id, text } => {
            assert_eq!(id, "req-uuid-1");
            assert_eq!(text, "yes please");
        }
        other => panic!("wrong variant: {other:?}"),
    }

    // 对应 event 也能 roundtrip(serde tag 是 `ask_user_input`,snake_case
    // 由 `EventMsg` 顶层 `#[serde(rename_all = "snake_case")]` 派生)。
    let ev_json = serde_json::json!({
        "type": "ask_user_input",
        "request_id": "req-uuid-1",
        "prompt": "Continue?"
    })
    .to_string();
    let ev: EventMsg = serde_json::from_str(&ev_json).unwrap();
    match ev {
        EventMsg::AskUserInput(AskUserInputEvent {
            request_id, prompt, ..
        }) => {
            assert_eq!(request_id, "req-uuid-1");
            assert_eq!(prompt, "Continue?");
        }
        other => panic!("wrong event variant: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_metadata_includes_prompt_response_elapsed() {
    // 场景 8:ToolOutput.metadata 契约:prompt / response / elapsed_ms 三字段。
    let tool = AskUserTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec =
        tokio::spawn(async move { tool.execute(ctx, serde_json::json!({"prompt": "hi"})).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent { request_id, .. }) => request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };
    assert!(complete_ask_user_input(&uw, &request_id, "hello".into()));

    let out = exec.await.unwrap().unwrap();
    assert!(!out.is_error);
    assert_eq!(out.metadata["prompt"], "hi");
    assert_eq!(out.metadata["response"], "hello");
    assert!(out.metadata["elapsed_ms"].is_u64());
    assert_eq!(out.elapsed_ms, out.metadata["elapsed_ms"].as_u64().unwrap());
}

#[tokio::test]
async fn ask_user_rejects_empty_prompt() {
    // v1.2 review P1:bug-3:空字符串 prompt(经 trim 后为空)。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": ""}))
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("empty") || message.contains("whitespace"));
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_rejects_whitespace_prompt() {
    // v1.2 review P1:bug-3:纯空白字符串。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": "   \t\n"}))
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("whitespace") || message.contains("empty"));
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_rejects_missing_prompt() {
    // v1.2 review P2:bug-5:空对象 / 缺 prompt。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool.execute(ctx, serde_json::json!({})).await.unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("prompt") || message.contains("keys"));
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_rejects_non_string_prompt() {
    // v1.2 review P2:bug-5:prompt 不是 string。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": 42}))
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("prompt") || message.contains("string"));
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_rejects_extra_fields() {
    // v1.2 review P2:bug-5:`additionalProperties: false` 运行时强制。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(
            ctx,
            serde_json::json!({"prompt": "hi", "extra": "junk", "another": 1}),
        )
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("unexpected key"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_rejects_oversized_prompt() {
    // v1.2 review P2:bug-2:`MAX_ASK_USER_PROMPT_BYTES = 16 KiB` 上限。
    let tool = AskUserTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    // 17 KiB prompt 触发拒。
    let big = "a".repeat(17 * 1024);
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": big}))
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("too long"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn ask_user_passes_through_prompt_with_leading_trailing_spaces() {
    // v1.2 review P1:bug-3:trim 不会拒绝合法 prompt —— "  hi  " 经 trim
    // 变 "hi",event 收到 "hi",response 不变。
    let tool = AskUserTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec = tokio::spawn(async move {
        tool.execute(ctx, serde_json::json!({"prompt": "  hi  "}))
            .await
    });
    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent {
            request_id, prompt, ..
        }) => {
            assert_eq!(prompt, "hi", "prompt should be trimmed");
            request_id
        }
        other => panic!("expected AskUserInput, got {other:?}"),
    };
    assert!(complete_ask_user_input(&uw, &request_id, "ok".into()));
    let out = exec.await.unwrap().unwrap();
    assert_eq!(out.metadata["prompt"], "hi");
    assert_eq!(out.metadata["response"], "ok");
}
