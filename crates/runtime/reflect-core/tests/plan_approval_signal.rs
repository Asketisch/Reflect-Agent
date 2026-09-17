//! v1.4 Plan 审批信号回传集成测试。
//!
//! 回归背景:v1.4 之前 `tool_exec` 在 `ExitPlanMode` 审批完成后**丢弃**用户
//! choice(`let _ = dispatch_plan_ready_blocking(...)`),模型收到的 Tool
//! 消息始终是工具原始输出 —— 真实 LLM 在工具返回后倾向直接结束 turn
//! ("Waiting for your approval to proceed."),已批准的 plan 永远不会被执行
//! (运行时真实 LLM 测试暴露)。修复后,决策被重写进工具输出第一个 text
//! block(模型可见的 Tool 消息 + latest_content):
//! - 批准(AutoMode / ManualApprove)→ "Plan approved. ... Execute the plan now..."
//! - Revise(3/Esc)→ "The user requested changes ... Stay in Plan mode..."
//!
//! 本测试用 stub LLM 驱动完整链路:首响 `ExitPlanMode` tool_call →
//! tool_exec 派发 `PlanReady` 并阻塞等审批 → 测试截获事件后投递
//! `Op::PlanApproval` → 断言后续模型请求历史里的 Tool 消息被重写。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_protocol::{EventMsg, Op, PlanApprovalChoice, Submission, UserInputItem};
use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::Value;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// ── LLM stub(记录每次 request,便于断言请求内容)────────────────

struct RecordingStub {
    /// 每次 `stream` 从队首弹出一批事件;空队列时返回纯文本收尾批。
    batches: Mutex<Vec<Vec<ChatEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl RecordingStub {
    fn new(batches: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            batches: Mutex::new(batches),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl ModelClient for RecordingStub {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            prompt_caching: true,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().push(request);
        let mut b = self.batches.lock();
        let events = if b.is_empty() {
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

/// `ExitPlanMode` 的无副作用替身:返回成功输出。tool_exec 的 plan 审批
/// 派发只认工具名 + `is_error`,不依赖真实工具实现。
struct ExitPlanModeStub;

#[async_trait]
impl Tool for ExitPlanModeStub {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }
    fn description(&self) -> &str {
        "stub ExitPlanMode"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(
                "Plan ready (12 chars)",
            )],
            is_error: false,
            metadata: Value::Null,
            elapsed_ms: 0,
        })
    }
}

// ── 辅助 ────────────────────────────────────────────────────────────

fn user_sub(id: &str, text: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        workspace: None,
        client_user_message_id: None,
        trace: None,
        source_command: None,
    }
}

fn plan_approval_sub(submission_id: &str, plan_id: &str, choice: PlanApprovalChoice) -> Submission {
    Submission {
        id: submission_id.into(),
        op: Op::PlanApproval {
            id: plan_id.into(),
            choice,
        },
        workspace: None,
        client_user_message_id: None,
        trace: None,
        source_command: None,
    }
}

/// 提取 ChatRequest 历史里所有 Tool(工具结果)消息的文本。
fn tool_result_text(req: &ChatRequest) -> String {
    let mut out = String::new();
    for m in &req.messages {
        if let ChatMessage::Tool(tr) = m {
            for b in &tr.content {
                if let reflect_llm::ContentBlock::Text { text } = b {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// 构造「首响 ExitPlanMode tool_call + 尾随文本」的 stub 批次。
fn exit_plan_mode_batch(msg_id: &str, tool_call_id: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::MessageStart {
            id: msg_id.into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("## Plan\n- step 1".into()),
        ChatEvent::ToolUseStart {
            id: tool_call_id.into(),
            name: "ExitPlanMode".into(),
            input_json: String::new(),
        },
        ChatEvent::ToolUseDelta("{}".into()),
        ChatEvent::MessageStop,
    ]
}

// ── 测试 ────────────────────────────────────────────────────────────

/// 批准(ManualApprove)→ 模型可见的 Tool 消息必须含
/// "Plan approved ... Execute the plan now" 重写。
#[tokio::test]
async fn plan_approval_approved_rewrites_model_tool_output() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        exit_plan_mode_batch("m1", "tc1"),
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("executing".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread.submit(user_sub("sub-plan", "make a plan")).await;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        match ev.msg {
            EventMsg::PlanReady(ready) => {
                let t = thread.clone();
                let pid = ready.plan_id.to_string();
                tokio::spawn(async move {
                    t.submit(plan_approval_sub(
                        "sub-approve",
                        &pid,
                        PlanApprovalChoice::ManualApprove,
                    ))
                    .await;
                });
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let reqs = stub.requests();
    assert!(
        reqs.len() >= 2,
        "审批后模型应继续下一轮(共 >=2 次调用);got {}",
        reqs.len()
    );
    let text = tool_result_text(&reqs[1]);
    assert!(
        text.contains("Plan approved"),
        "批准后模型可见的 Tool 消息应含 'Plan approved' 重写;got: {text}"
    );
    assert!(
        text.contains("Execute the plan now"),
        "批准信号应指示模型立即执行 plan;got: {text}"
    );
}

/// Revise(3/Esc)→ 模型可见的 Tool 消息必须含 "The user requested changes"
/// 重写,且模型继续 turn(重新规划);最终批准时 Tool 消息再被批准文案重写。
#[tokio::test]
async fn plan_approval_revise_rewrites_then_approval_continues() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        // 第 1 轮:ExitPlanMode → 被 Revise。
        exit_plan_mode_batch("m1", "tc1"),
        // 第 2 轮:模型重新规划,再次 ExitPlanMode。
        exit_plan_mode_batch("m2", "tc2"),
        // 第 3 轮:正常文本收尾。
        vec![
            ChatEvent::MessageStart {
                id: "m3".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread.submit(user_sub("sub-plan", "make a plan")).await;
    let mut plan_ready_count = 0u32;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        match ev.msg {
            EventMsg::PlanReady(ready) => {
                plan_ready_count += 1;
                // 第一次 PlanReady → Revise;第二次 → 批准。
                let choice = if plan_ready_count == 1 {
                    PlanApprovalChoice::Revise
                } else {
                    PlanApprovalChoice::ManualApprove
                };
                let t = thread.clone();
                let pid = ready.plan_id.to_string();
                tokio::spawn(async move {
                    t.submit(plan_approval_sub("sub-approve", &pid, choice))
                        .await;
                });
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let reqs = stub.requests();
    assert!(
        reqs.len() >= 3,
        "Revise 后模型应重新规划并再次提交 plan(共 >=3 次调用);got {}",
        reqs.len()
    );
    // 第 2 次请求(Revise 之后):Tool 消息应含 Revise 重写文案。
    let revise_text = tool_result_text(&reqs[1]);
    assert!(
        revise_text.contains("The user requested changes"),
        "Revise 后模型可见的 Tool 消息应含修改提示;got: {revise_text}"
    );
    // 第 3 次请求(第二次批准之后):历史里同时保留 Revise 文案(第 1 次
    // ExitPlanMode 的结果)与批准文案(第 2 次)。
    let final_text = tool_result_text(&reqs[2]);
    assert!(
        final_text.contains("Plan approved"),
        "第二次批准后模型可见历史应含 'Plan approved' 重写;got: {final_text}"
    );
    assert!(
        final_text.contains("The user requested changes"),
        "Revise 重写应留在历史中(同 turn 内两条工具结果);got: {final_text}"
    );
}
