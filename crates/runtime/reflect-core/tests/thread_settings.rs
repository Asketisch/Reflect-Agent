//! v1.5 R1 — `ThreadSettingsOverrides` 诚实化端到端。
//!
//! `Op::UserInput.thread_settings` 的三个字段此前是死信,现在逐一消费:
//! - `approval_policy = deny`:本回合需审批工具一律拒(不弹 modal、
//!   bash 不执行),无需会话级 approvals 开关;
//! - `approval_policy = prompt`:会话级审批关闭时强制本回合弹审批,
//!   回执 Approve 后工具正常执行;
//! - `sandbox_policy = os_sandbox`:env 关沙箱时,本回合强制启用 OS
//!   沙箱(写系统目录被内核拒绝)。
//!
//! 模型用脚本化 stub(回放工具调用 → 文本收口),与 full_link 同款。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, Submission, ThreadSettingsOverrides, UserInputItem};
use reflect_tools::ToolRegistry;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct ScriptedClient {
    scripts: std::sync::Mutex<Vec<Vec<ChatEvent>>>,
}

#[async_trait]
impl ModelClient for ScriptedClient {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let next = {
            let mut q = self.scripts.lock().unwrap();
            if q.is_empty() {
                vec![
                    ChatEvent::MessageStart {
                        id: "m".into(),
                        model: "scripted".into(),
                    },
                    ChatEvent::MessageStop,
                ]
            } else {
                q.remove(0)
            }
        };
        Ok(Box::pin(stream::iter(
            next.into_iter().map(Ok::<ChatEvent, LlmError>),
        )))
    }
}

fn tool_call_script(id: &str, name: &str, args: serde_json::Value) -> Vec<ChatEvent> {
    vec![
        ChatEvent::MessageStart {
            id: "m".into(),
            model: "scripted".into(),
        },
        ChatEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
            input_json: String::new(),
        },
        ChatEvent::ToolUseDelta(args.to_string()),
        ChatEvent::MessageStop,
    ]
}

fn sub_with_settings(id: &str, text: &str, settings: ThreadSettingsOverrides) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: settings,
        },
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

fn sub(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

fn build_thread(client: Arc<ScriptedClient>) -> AgentThread {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "scripted",
        CredentialPool {
            entries: vec![PoolEntry {
                client,
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(reflect_tools::builtins::BashTool));
    // 会话级审批保持关闭:验证 overrides 自身的强制语义。
    let cfg = AgentConfig::new("scripted/m1", Path::new("."));
    AgentThread::new(cfg, registry, tools, None, None)
}

async fn drain_until_terminal(
    handle: &mut reflect_core::TurnHandle,
) -> Vec<reflect_protocol::Event> {
    let mut events = Vec::new();
    let deadline = Duration::from_secs(15);
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event in time")
    {
        let terminal = matches!(ev.msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_));
        events.push(ev);
        if terminal {
            break;
        }
    }
    events
}

/// approval_policy = deny:需审批工具(bash)被直接拒,不弹 ApprovalRequest。
#[tokio::test]
async fn deny_policy_rejects_prompt_tool_without_modal() {
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![
            tool_call_script(
                "c1",
                "bash",
                serde_json::json!({"cmd": "echo should-not-run"}),
            ),
            // 模型第二次调用(工具被拒后仍会请求模型收口)。
            vec![
                ChatEvent::MessageStart {
                    id: "m2".into(),
                    model: "scripted".into(),
                },
                ChatEvent::ContentDelta("工具被拒,我说明情况".into()),
                ChatEvent::MessageStop,
            ],
        ]),
    });
    let thread = build_thread(client);

    let settings = ThreadSettingsOverrides {
        approval_policy: Some(reflect_protocol::ApprovalPolicy::Deny),
        ..Default::default()
    };
    let mut h = thread
        .submit(sub_with_settings("d1", "跑命令", settings))
        .await;
    let events = drain_until_terminal(&mut h).await;

    // 不应有任何 ApprovalRequest(不需要 modal)。
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.msg, EventMsg::ApprovalRequest(_))),
        "deny 策略不应弹审批"
    );
    // 工具调用以 error 结束,原因来自 deny-all。
    let denied = events.iter().any(|e| match &e.msg {
        EventMsg::ToolCallEnd(end) => end.is_error,
        _ => false,
    });
    assert!(denied, "bash 应以错误结束");
    let denied_text = events
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::ToolCallEnd(end) => Some(
                end.output
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .any(|t| t.contains("deny"));
    assert!(denied_text, "拒绝原因应含 deny 语义");
    assert!(matches!(
        events.last().map(|e| &e.msg),
        Some(EventMsg::TurnComplete(_))
    ));
}

mod common;

/// approval_policy = prompt:会话级关闭时强制弹审批;Approve 回执后工具执行。
#[tokio::test]
async fn prompt_policy_forces_approval_modal_then_approves() {
    // bash 工具会真实 spawn 子进程:GHA runner 的 landlock 异常环境
    // (见 common::exec_capable)下跳过 —— 环境限制,非本链路回归。
    if !common::exec_capable() {
        eprintln!("skip: landlock restrict 后 exec 不可用(runner 环境限制)");
        return;
    }
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![
            tool_call_script(
                "c1",
                "bash",
                serde_json::json!({"cmd": "echo approved-run"}),
            ),
            vec![
                ChatEvent::MessageStart {
                    id: "m2".into(),
                    model: "scripted".into(),
                },
                ChatEvent::ContentDelta("ok".into()),
                ChatEvent::MessageStop,
            ],
        ]),
    });
    let thread = build_thread(client);

    let settings = ThreadSettingsOverrides {
        approval_policy: Some(reflect_protocol::ApprovalPolicy::Prompt),
        ..Default::default()
    };
    let mut h = thread
        .submit(sub_with_settings("p1", "跑命令", settings))
        .await;

    // 等审批弹窗。
    let deadline = Duration::from_secs(10);
    let mut request_id = None;
    while request_id.is_none() {
        let ev = timeout(deadline, h.next())
            .await
            .expect("event in time")
            .expect("channel open");
        if let EventMsg::ApprovalRequest(a) = ev.msg {
            request_id = Some(a.request_id);
        }
    }
    let request_id = request_id.expect("应弹出 ApprovalRequest");

    // 回执 Approve(会话级 approvals 关闭时 gate 仍存在 —— overrides 强制)。
    let _ack = thread
        .submit(sub(
            "p1-ack",
            Op::ToolApproval {
                id: request_id,
                decision: reflect_protocol::ReviewDecision::Approve,
            },
        ))
        .await;

    let events = drain_until_terminal(&mut h).await;
    let approved = events.iter().any(|e| match &e.msg {
        EventMsg::ToolCallEnd(end) => {
            !end.is_error
                && end
                    .output
                    .content
                    .iter()
                    .any(|b| matches!(b, reflect_protocol::ContentBlock::Text { text } if text.contains("approved-run")))
        }
        _ => false,
    });
    assert!(approved, "Approve 回执后 bash 应执行成功");
}

/// sandbox_policy = os_sandbox:env 关沙箱时,本回合强制 OS 沙箱 ——
/// 写系统目录被内核拒绝(仅 macOS 有 Seatbelt 后端)。
#[cfg(target_os = "macos")]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn sandbox_policy_os_forces_seatbelt_per_turn() {
    // 与 bash 单测一致:env 锁串行化,整个测试期 env 关沙箱。
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap();
    let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
    let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
    unsafe {
        std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
        std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
    }
    let restore = |prior_on: Option<String>, prior_strict: Option<String>| {
        match prior_on {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
        }
        match prior_strict {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
        }
    };

    // Seatbelt 放行 workspace+tmp;$HOME 用户可写但不在放行清单 ——
    // 正好区分「无沙箱可写」与「有沙箱拒写」。
    let marker = format!(
        "{}/.reflect-sb-probe",
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
    );
    let cmd = format!("touch {marker} 2>/dev/null; test -f {marker} && echo WROTE || echo BLOCKED");
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![
            // turn1:调工具 → 工具后收口(两段,防止队列串位)。
            tool_call_script("c1", "bash", serde_json::json!({"cmd": cmd})),
            vec![
                ChatEvent::MessageStart {
                    id: "m1".into(),
                    model: "scripted".into(),
                },
                ChatEvent::MessageStop,
            ],
            // turn2:os_sandbox 覆盖 → 应 BLOCKED。
            tool_call_script("c2", "bash", serde_json::json!({"cmd": cmd})),
            vec![
                ChatEvent::MessageStart {
                    id: "m2".into(),
                    model: "scripted".into(),
                },
                ChatEvent::MessageStop,
            ],
        ]),
    });
    let thread = build_thread(client);

    // turn1:无覆盖(env 已关)→ 无沙箱 → 写成功。
    let mut h1 = thread
        .submit(sub_with_settings(
            "s1",
            "写系统目录",
            ThreadSettingsOverrides::default(),
        ))
        .await;
    let ev1 = drain_until_terminal(&mut h1).await;
    let t1 = ev1
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::ToolCallEnd(end) => Some(
                end.output
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    let _ = std::fs::remove_file(marker);
    assert!(t1.contains("WROTE"), "env 关沙箱时应可写: {t1}");

    // turn2:os_sandbox 覆盖 → Seatbelt 拒写。
    let settings = ThreadSettingsOverrides {
        sandbox_policy: Some(reflect_protocol::SandboxPolicy::OsSandbox),
        ..Default::default()
    };
    let mut h2 = thread
        .submit(sub_with_settings("s2", "再写系统目录", settings))
        .await;
    let ev2 = drain_until_terminal(&mut h2).await;
    restore(prior_on, prior_strict);

    let t2 = ev2
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::ToolCallEnd(end) => Some(
                end.output
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    assert!(
        t2.contains("BLOCKED"),
        "os_sandbox 覆盖应拒写系统目录: {t2}"
    );
}
