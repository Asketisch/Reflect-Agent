//! M6.1.4 — 端到端审批路由集成测试。
//!
//! 每个测试构造一个 `approvals: true` 并注册了 `Prompt` 权限 stub 工具的
//! `AgentThread`。LLM stub 为该工具发出 `tool_use`;queue 在
//! `ApprovalGate::ask_tool` 上阻塞;测试排空 `TurnHandle` 直到看见
//! `ApprovalRequest`,然后提交匹配的 `Op::ToolApproval` 并校验工具运行
//! 或被拒绝。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, PermissionMode, ReviewDecision, Submission, UserInputItem};
use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::Value;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// ── LLM stub(桩实现)──────────────────────────────────────────

struct StubClient {
    /// 每次调用 `stream` 从队首弹出一批事件。
    batches: Mutex<Vec<Vec<ChatEvent>>>,
}

impl StubClient {
    fn new(batches: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            batches: Mutex::new(batches),
        }
    }
}

#[async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let mut b = self.batches.lock();
        let events = if b.is_empty() {
            // 尾部空批 —— 终止回合。
            vec![
                ChatEvent::MessageStart {
                    id: "done".into(),
                    model: "stub-1".into(),
                },
                ChatEvent::MessageStop,
            ]
        } else {
            b.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

// ── 需要 Prompt 审批的 stub 工具 ────────────────────────────────────

/// 一个无副作用、并发不安全的工具,要求 `Prompt` 权限。用于触发审批 gate,
/// 而无需拖入 `bash`。
struct PromptStub;

#[async_trait]
impl Tool for PromptStub {
    fn name(&self) -> &str {
        "prompt_stub"
    }
    fn description(&self) -> &str {
        "stub tool that requires approval"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }
    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text("prompt_stub ran")],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

// ── 辅助函数 ────────────────────────────────────────────────────────────

fn build_thread_with_approvals(registry: Arc<ModelRegistry>) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(PromptStub));
    AgentThread::new(cfg, registry, tools, None, None)
}

fn user_sub(id: &str, text: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
    }
}

fn approval_sub(submission_id: &str, request_id: &str, decision: ReviewDecision) -> Submission {
    Submission {
        id: submission_id.into(),
        op: Op::ToolApproval {
            id: request_id.into(),
            decision,
        },
        client_user_message_id: None,
        trace: None,
    }
}

/// 一段 LLM 批次:先触发 `prompt_stub` tool_use,再尾随一段
/// 收尾(关闭 turn)。
fn tool_use_then_done() -> Vec<Vec<ChatEvent>> {
    vec![
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "prompt_stub".into(),
                input_json: String::new(),
            },
            ChatEvent::ToolUseDelta("{}".into()),
            ChatEvent::MessageStop,
        ],
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("ok".into()),
            ChatEvent::MessageStop,
        ],
    ]
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approve_unblocks_prompt_required_tool() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = Arc::new(build_thread_with_approvals(registry));

    let sub_id = "sub-approve";
    let mut handle = thread.submit(user_sub(sub_id, "run prompt_stub")).await;

    let mut saw_tool_ok = false;
    while let Some(ev) = timeout(Duration::from_secs(3), handle.next())
        .await
        .expect("event arrives")
    {
        match ev.msg {
            EventMsg::ApprovalRequest(req) => {
                let t = thread.clone();
                let rid = req.request_id.clone();
                let sid = sub_id.to_string();
                tokio::spawn(async move {
                    t.submit(approval_sub(&sid, &rid, ReviewDecision::Approve))
                        .await;
                });
            }
            EventMsg::ToolCallEnd(end) => {
                assert!(!end.is_error, "tool should have run cleanly after approve");
                saw_tool_ok = true;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(
        saw_tool_ok,
        "expected the approved tool to produce a clean ToolCallEnd"
    );
}

#[tokio::test]
async fn deny_returns_error_result() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = Arc::new(build_thread_with_approvals(registry));

    let sub_id = "sub-deny";
    let mut handle = thread.submit(user_sub(sub_id, "run prompt_stub")).await;

    let mut saw_denied = false;
    while let Some(ev) = timeout(Duration::from_secs(3), handle.next())
        .await
        .expect("event arrives")
    {
        match ev.msg {
            EventMsg::ApprovalRequest(req) => {
                let t = thread.clone();
                let rid = req.request_id.clone();
                let sid = sub_id.to_string();
                tokio::spawn(async move {
                    t.submit(approval_sub(
                        &sid,
                        &rid,
                        ReviewDecision::Deny {
                            reason: "no thanks".into(),
                        },
                    ))
                    .await;
                });
            }
            EventMsg::ToolCallEnd(end) => {
                assert!(end.is_error, "tool should have been denied");
                let text = end.output.content.iter().find_map(|b| match b {
                    reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                });
                assert!(
                    text.as_deref().unwrap_or("").contains("Approval denied"),
                    "missing denial reason in: {:?}",
                    text
                );
                saw_denied = true;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(saw_denied, "expected an error ToolCallEnd after deny");
}

#[tokio::test]
async fn approval_disabled_when_config_off() {
    // `approvals: false` 时 gate 不安装;即使工具是 `Prompt` 权限,
    // 也不经任何审批往返直接执行。
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(PromptStub));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(user_sub("sub-off", "go")).await;
    let mut saw_approval_request = false;
    let mut saw_tool_end = false;
    while let Some(ev) = timeout(Duration::from_secs(3), handle.next())
        .await
        .expect("event arrives")
    {
        match ev.msg {
            EventMsg::ApprovalRequest(_) => saw_approval_request = true,
            EventMsg::ToolCallEnd(end) => {
                assert!(!end.is_error);
                saw_tool_end = true;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(
        !saw_approval_request,
        "no gate should mean no ApprovalRequest"
    );
    assert!(saw_tool_end, "tool should still have run");
}

#[tokio::test]
async fn stale_approval_op_is_silently_dropped() {
    // 对未知 request_id 提交 Op::ToolApproval 不应 panic 或令
    // submission loop 崩溃。后续 UserInput 仍能正常处理。
    let registry = Arc::new(ModelRegistry::new());
    // 纯文本响应 —— 无 tool_use,不触发审批 gate。
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::ContentDelta("hello".into()),
                    ChatEvent::MessageStop,
                ]])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread_with_approvals(registry);

    // 发一条过期(失效)的审批。
    let _ = thread
        .submit(approval_sub(
            "sub-stale",
            "no-such-request",
            ReviewDecision::Approve,
        ))
        .await;

    // 线程仍存活,能响应下一条 UserInput。
    let mut handle = thread.submit(user_sub("sub-after", "hi")).await;
    let mut saw_complete = false;
    while let Some(ev) = timeout(Duration::from_secs(3), handle.next())
        .await
        .expect("event arrives")
    {
        if let EventMsg::TurnComplete(_) = ev.msg {
            saw_complete = true;
            break;
        }
    }
    assert!(saw_complete);
}

/// 回归:复刻 TUI 提交 pump 的「单任务 select!」结构(见
/// `reflect-tui/src/app.rs` Producer 4)。同一个 task 既要排空 turn handle,
/// 又要在 turn 进行中派发审批提交。旧的串行 `while handle.next() { .. }` +
/// `while sub_chan.recv()` 结构会在 graph park 于审批 oneshot 时死锁
/// (handle 永不到 None,审批提交永远不被消费)。select! 让两者并发。
///
/// 本测试用真实引擎 + stub 模型端到端验证:看到 ApprovalRequest 后把审批
/// 回复写入 sub_chan,pump 分支 B 派发它,graph 恢复,ToolCallEnd +
/// TurnComplete 仍由 **原 handle** 投递。
#[tokio::test]
async fn tui_pump_style_select_does_not_deadlock_on_approval() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = Arc::new(build_thread_with_approvals(registry));

    let sub_id = "pump";
    let mut handle = thread.submit(user_sub(sub_id, "run prompt_stub")).await;

    // 模拟 TUI 的 outbound Submission 通道(reducer → pump)。
    let (sub_tx, mut sub_chan) = tokio::sync::mpsc::unbounded_channel::<Submission>();

    let mut saw_approval = false;
    let mut saw_tool_ok = false;
    loop {
        tokio::select! {
            // 分支 A:排空当前 turn 的事件。
            maybe_ev = handle.next() => {
                let ev = match timeout(Duration::from_secs(5), async { maybe_ev })
                    .await
                    .expect("event arrives within 5s")
                {
                    Some(ev) => ev,
                    None => break, // turn 结束
                };
                match ev.msg {
                    EventMsg::ApprovalRequest(req) => {
                        saw_approval = true;
                        // 模拟 on_key_approval → emit_submission:写进 sub_chan。
                        let _ = sub_tx.send(approval_sub(
                            sub_id,
                            &req.request_id,
                            ReviewDecision::Approve,
                        ));
                    }
                    EventMsg::ToolCallEnd(end) => {
                        assert!(!end.is_error, "tool should run cleanly after approve");
                        saw_tool_ok = true;
                    }
                    EventMsg::TurnComplete(_) => break,
                    _ => {}
                }
            }
            // 分支 B:turn 进行中来了控制类提交 → 派发并丢弃其 handle。
            // 旧串行 pump 永远到不了这里(死锁),select! 使其可达。
            maybe_ctrl = sub_chan.recv() => {
                if let Some(ctrl_sub) = maybe_ctrl {
                    let _dropped = thread.submit(ctrl_sub).await;
                }
            }
        }
    }
    assert!(saw_approval, "应先看到 ApprovalRequest");
    assert!(saw_tool_ok, "审批后工具应成功执行(证明未死锁)");
}

// ── Bypass 模式:静默跳过工具审批 ──────
//
// 覆盖:
// 1. Bypass 下 Prompt 工具直接跑,无 ApprovalRequest modal。
// 2. Bypass 下 ask_user 仍弹出(工具执行审批 ≠ 主动索取人类输入)。

/// 一个发出 `ask_user` tool_use 的 LLM 批次,用于触发 AskUserTool。
fn ask_user_tool_use_then_done() -> Vec<Vec<ChatEvent>> {
    vec![
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "ask_user".into(),
                input_json: String::new(),
            },
            ChatEvent::ToolUseDelta(r#"{"prompt":"Your name?"}"#.into()),
            ChatEvent::MessageStop,
        ],
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("ok".into()),
            ChatEvent::MessageStop,
        ],
    ]
}

/// Bypass 兼容模式下,Prompt 工具仍必须经过审批。
#[tokio::test]
async fn bypass_mode_still_requires_approval() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = Arc::new(build_thread_with_approvals(registry));
    // Bypass 仅保留协议兼容性，不再静默跳过工具审批。
    thread.config().set_permission_mode(PermissionMode::Bypass);

    let mut handle = thread
        .submit(user_sub("sub-bypass", "run prompt_stub"))
        .await;
    let mut saw_approval = false;
    while let Some(ev) = timeout(Duration::from_secs(3), handle.next())
        .await
        .expect("approval event arrives")
    {
        if matches!(ev.msg, EventMsg::ApprovalRequest(_)) {
            saw_approval = true;
            break;
        }
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    assert!(saw_approval, "Bypass 不应绕过 Prompt 工具审批");
}

/// Bypass 模式下,`ask_user` 工具仍 emit `AskUserInput`(主动索取人类输入
/// 不被工具审批短路跳过)。验证后用 cancel token 终结等待。
#[tokio::test]
async fn bypass_mode_still_asks_user_questions() {
    use reflect_tools::builtins::AskUserTool;

    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(ask_user_tool_use_then_done())),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(AskUserTool));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));
    // 进入 Bypass:工具审批被静默跳过,但 ask_user 不受影响。
    thread.config().set_permission_mode(PermissionMode::Bypass);
    // 复制 cancel token,看到 AskUserInput 后 cancel 终结 ask_user 等待。
    let cancel = thread.config().cancel.clone();

    let mut handle = thread.submit(user_sub("sub-bypass-ask", "ask me")).await;
    let mut saw_ask_user_input = false;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        match ev.msg {
            EventMsg::AskUserInput(_) => {
                saw_ask_user_input = true;
                // ask_user 已就位;cancel 让 gate.ask_user 返回 Cancelled,
                // 工具 ToolCallEnd(is_error) 后 turn 收尾。
                cancel.cancel();
            }
            EventMsg::ApprovalRequest(_) => {
                panic!("Bypass 不应对 ask_user emit ApprovalRequest");
            }
            EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(
        saw_ask_user_input,
        "Bypass 下 ask_user 仍应 emit AskUserInput(主动提问不被工具审批跳过)"
    );
}
