//! v1.2 review P2:bug-7:`request_human_input` 端到端测试(新文件)。
//!
//! 覆盖场景(每场景独立测试,无共享状态):
//! 1. happy path:emit event + 等回执 + 拿回答案 + metadata 含 `context_id`。
//! 2. `context_id` 缺省时为字符串 `"default"`。
//! 3. 空 / 纯空白 prompt → `InvalidArgs`。
//! 4. headless 模式(无 ApprovalGate)→ `Execution`。
//! 5. cancel 短路 → `Cancelled`。
//! 6. event channel 关闭 → `Execution`。
//! 7. 额外字段(违反 `additionalProperties: false`)→ `InvalidArgs`。
//! 8. `persistent: true` 走 `timeout_secs = 0` 永不超时分支(回归契约)。

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reflect_protocol::{AskUserInputEvent, ContentBlock, Event, EventMsg, ToolError};
use reflect_tools::approval::ApprovalGate;
use reflect_tools::builtins::RequestHumanInputTool;
use reflect_tools::tool::{Tool, ToolContext};
use reflect_tools::{AskUserInputWaiters, complete_ask_user_input};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn make_ctx_with_shared_uw() -> (ToolContext, mpsc::Receiver<Event>, AskUserInputWaiters) {
    let (tx, rx) = mpsc::channel::<Event>(8);
    let uw: AskUserInputWaiters = Arc::new(Mutex::new(Default::default()));
    let gate = Arc::new(ApprovalGate::with_state(
        tx,
        "sub-rhi",
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
async fn happy_path_returns_text_with_context_id() {
    // 场景 1:tool 调 execute,emit AskUserInput,等回执,返回文本。
    let tool = RequestHumanInputTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec = tokio::spawn(async move {
        tool.execute(
            ctx,
            serde_json::json!({"prompt": "Pick context", "context_id": "ci-1"}),
        )
        .await
    });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent {
            request_id, prompt, ..
        }) => {
            assert_eq!(prompt, "Pick context");
            request_id
        }
        other => panic!("expected AskUserInput, got {other:?}"),
    };
    assert!(complete_ask_user_input(&uw, &request_id, "answer".into()));

    let out = exec.await.unwrap().unwrap();
    assert!(!out.is_error);
    match &out.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "answer"),
        other => panic!("expected Text, got {other:?}"),
    }
    // metadata 契约:context_id + persistent=true + elapsed_ms。
    assert_eq!(out.metadata["context_id"], "ci-1");
    assert_eq!(out.metadata["persistent"], true);
    assert_eq!(out.metadata["response"], "answer");
    assert!(out.metadata["elapsed_ms"].is_u64());
}

#[tokio::test]
async fn default_context_id_is_string_default() {
    // 场景 2:不传 `context_id` → metadata 标记 `"default"`。
    let tool = RequestHumanInputTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec =
        tokio::spawn(async move { tool.execute(ctx, serde_json::json!({"prompt": "x"})).await });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent { request_id, .. }) => request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };
    assert!(complete_ask_user_input(&uw, &request_id, "y".into()));

    let out = exec.await.unwrap().unwrap();
    assert_eq!(out.metadata["context_id"], "default");
}

#[tokio::test]
async fn rejects_empty_prompt() {
    // 场景 3:空 prompt(经 trim)→ InvalidArgs。
    let tool = RequestHumanInputTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": "   "}))
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
async fn headless_without_gate_returns_execution_error() {
    // 场景 4:无 ApprovalGate(headless)→ Execution。
    let tool = RequestHumanInputTool;
    let err = tool
        .execute(ToolContext::default(), serde_json::json!({"prompt": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn cancellation_propagates_cancelled_error() {
    // 场景 5:cancel token 触发 → Cancelled(永不超时分支不依赖
    // 事件 receive,所以 cancel 仍能终结)。
    let tool = RequestHumanInputTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let cancel = ctx.cancel.clone();
    let exec = tokio::spawn(async move {
        tool.execute(ctx, serde_json::json!({"prompt": "long"}))
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
async fn closed_event_channel_returns_execution_error() {
    // 场景 6:event channel 已 drop → Execution。
    let (tx, rx) = mpsc::channel::<Event>(8);
    drop(rx);
    let uw: AskUserInputWaiters = Arc::new(Mutex::new(Default::default()));
    let gate = Arc::new(ApprovalGate::with_state(
        tx,
        "sub-rhi-closed",
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
    let tool = RequestHumanInputTool;
    let err = tool
        .execute(ctx, serde_json::json!({"prompt": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
}

#[tokio::test]
async fn rejects_extra_fields() {
    // 场景 7:超出 prompt + context_id 两个允许字段 → InvalidArgs。
    let tool = RequestHumanInputTool;
    let (ctx, _rx, _uw) = make_ctx_with_shared_uw();
    let err = tool
        .execute(
            ctx,
            serde_json::json!({
                "prompt": "x",
                "context_id": "ci-1",
                "extra": "junk",
            }),
        )
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("unexpected keys"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got: {other:?}"),
    }
}

#[tokio::test]
async fn persistent_disables_timeout_waits_for_user_response() {
    // 场景 8:`timeout_secs = 0` 永不超时分支契约 —— 200ms 内不返回
    // (既不超时自杀,也不通过 cancel 提前终结),用户响应后正常完成。
    let tool = RequestHumanInputTool;
    let (ctx, mut rx, uw) = make_ctx_with_shared_uw();
    let exec = tokio::spawn(async move {
        tool.execute(
            ctx,
            serde_json::json!({"prompt": "long task", "context_id": "ci-persistent"}),
        )
        .await
    });

    let ev = rx.recv().await.unwrap();
    let request_id = match ev.msg {
        EventMsg::AskUserInput(AskUserInputEvent { request_id, .. }) => request_id,
        other => panic!("expected AskUserInput, got {other:?}"),
    };

    // 200ms 确认 tool 没有 timeout 自杀(timeout=0 永不超时)。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !exec.is_finished(),
        "request_human_input should never self-terminate (timeout=0)"
    );

    // 用户响应后正常完成。
    assert!(complete_ask_user_input(&uw, &request_id, "ship it".into()));
    let out = exec.await.unwrap().unwrap();
    assert_eq!(out.metadata["response"], "ship it");
    assert_eq!(out.metadata["context_id"], "ci-persistent");
}
