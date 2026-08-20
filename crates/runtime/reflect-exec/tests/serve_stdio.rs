//! `reflect serve` 的 stdio JSONL 协议集成测试。
//!
//! 用 tokio duplex 内存管道直接驱动 [`reflect_exec::serve::serve_session`],
//! 配合内置 mock provider(脚本化回复)**全程离线**,覆盖:
//! 握手(session_configured)、多轮、远程工具注册 / 请求 / 回执、
//! Shutdown 优雅退出、坏行容错。

use std::sync::Arc;
use std::time::Duration;

use reflect_core::{AgentConfig, AgentThread};
use reflect_exec::serve::serve_session;
use reflect_llm::{ChatEvent, CredentialPool, MockClient, MockReply, ModelRegistry, PoolEntry};
use reflect_protocol::{ContentBlock, Event, EventMsg, Op, RemoteToolSpec, Submission, ToolOutput};
use reflect_tools::ToolRegistry;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

/// 测试内统一等待上限(事件到达快,宽松上限只为防挂死)。
const WAIT: Duration = Duration::from_secs(30);

/// 构造注册了脚本化 mock provider 的 registry。
fn mock_registry(script: Vec<MockReply>) -> Arc<ModelRegistry> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "mock",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(MockClient::with_script(script)),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    registry
}

/// 最小 AgentThread:mock 模型 + 空工具表(远程工具经 serve 注册)。
/// 不挂 hook / sanitizer —— 单测聚焦 serve 协议回路。
fn minimal_thread(script: Vec<MockReply>) -> Arc<AgentThread> {
    let dir = std::env::temp_dir().join(format!("reflect-serve-test-{}", std::process::id()));
    Arc::new(AgentThread::new(
        AgentConfig::new("mock/mock-1", dir),
        mock_registry(script),
        Arc::new(ToolRegistry::default()),
        None,
        None,
    ))
}

/// 启动一个 serve 会话,返回 (客户端写端, 事件接收端)。
///
/// 两对 duplex 隔离读写,避免 split 后类型不匹配的繁琐:
/// - 一对 `(client_w → server_r)`:客户端写 Submission、serve 读;
/// - (server_w 端被丢弃 —— serve 不写回这条 pipe)。
async fn start_serve(
    thread: Arc<AgentThread>,
) -> (
    DuplexStream,
    mpsc::Receiver<Event>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (sink, sink_rx) = mpsc::channel::<Event>(256);
    let tools = thread.tools().clone();
    let handle = tokio::spawn(serve_session(thread, tools, server_r, sink));
    (client_w, sink_rx, handle)
}

/// 向 serve 写一行 Submission JSON。
async fn send_sub(w: &mut DuplexStream, sub: &Submission) {
    let mut line = serde_json::to_string(sub).unwrap();
    line.push('\n');
    w.write_all(line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

/// 从事件流里等第一条满足谓词的事件(超时 panic,附已见事件辅助排障)。
async fn expect_event(
    rx: &mut mpsc::Receiver<Event>,
    mut pred: impl FnMut(&EventMsg) -> bool,
) -> EventMsg {
    let mut seen: Vec<String> = Vec::new();
    loop {
        let ev = tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("等待事件超时")
            .expect("事件通道提前关闭");
        seen.push(ev.msg.discriminant().to_string());
        if pred(&ev.msg) {
            return ev.msg;
        }
    }
}

#[tokio::test]
async fn serve_全链路_多轮_远程工具_优雅关闭() {
    // 脚本:第 1 轮文本;第 2 轮先调远程工具,拿到结果后再回文本。
    let thread = minimal_thread(vec![
        MockReply::Text {
            text: "你好,我是 mock".into(),
        },
        MockReply::ToolCall {
            name: "get_weather".into(),
            args: serde_json::json!({"city": "北京"}),
        },
        MockReply::Text {
            text: "北京今天晴".into(),
        },
    ]);
    let (mut w, mut rx, serve_task) = start_serve(thread).await;

    // ── 1. 首轮:握手 + 文本回合 ──
    send_sub(&mut w, &Submission::user_input("打个招呼")).await;
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::SessionConfigured(_))).await;
    let EventMsg::SessionConfigured(cfg) = msg else {
        unreachable!()
    };
    assert_eq!(cfg.provider, "mock");
    assert_eq!(cfg.model, "mock/mock-1");

    // 收集所有文本增量直到 TurnComplete,断言含 mock provider 标识。
    let mut collected = String::new();
    loop {
        let msg = expect_event(&mut rx, |m| {
            matches!(
                m,
                EventMsg::AgentMessageDelta(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        match msg {
            EventMsg::AgentMessageDelta(d) => collected.push_str(&d.delta),
            EventMsg::TurnComplete(_) => break,
            _ => unreachable!(),
        }
    }
    assert!(collected.contains("mock"), "got: {collected}");

    // ── 2. 注册远程工具 + 第二轮:LLM 调用 → 客户端执行 → 回执 ──
    let register = Submission::with_id(
        "reg-1",
        Op::RegisterTools {
            tools: vec![RemoteToolSpec {
                name: "get_weather".into(),
                description: "查询城市天气".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }),
            }],
        },
    );
    send_sub(&mut w, &register).await;

    send_sub(&mut w, &Submission::user_input("北京天气如何?")).await;

    // 引擎应发起远程工具执行请求(参数透传)。
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::ToolExecutionRequest(_))).await;
    let EventMsg::ToolExecutionRequest(req) = msg else {
        unreachable!()
    };
    assert_eq!(req.tool, "get_weather");
    assert_eq!(req.args["city"], "北京");

    // 客户端本地执行(这里直接构造结果)并回执。
    let reply = Submission::with_id(
        "resp-1",
        Op::ToolExecutionResponse {
            call_id: req.call_id.clone(),
            output: ToolOutput {
                content: vec![ContentBlock::text("晴,26℃")],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 1,
            },
        },
    );
    send_sub(&mut w, &reply).await;

    // 工具调用以 ToolCallEnd 收尾(非错误)。
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::ToolCallEnd(_))).await;
    let EventMsg::ToolCallEnd(end) = msg else {
        unreachable!()
    };
    assert!(!end.is_error, "远程工具应成功返回");

    // 第二次模型调用(脚本第 3 行)产出收尾文本,turn 完成。
    // 第二次模型调用(脚本第 3 行)产出收尾文本,turn 完成。文本可能
    // 拆成多个 delta,收齐再断言。
    let mut second_text = String::new();
    loop {
        let msg = expect_event(&mut rx, |m| {
            matches!(
                m,
                EventMsg::AgentMessageDelta(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        match msg {
            EventMsg::AgentMessageDelta(d) => second_text.push_str(&d.delta),
            EventMsg::TurnComplete(_) => break,
            _ => unreachable!(),
        }
    }
    assert!(second_text.contains("今天晴"), "got: {second_text}");

    // ── 3. Shutdown:优雅退出 + ShutdownComplete ──
    send_sub(&mut w, &Submission::with_id("bye", Op::Shutdown)).await;
    expect_event(&mut rx, |m| matches!(m, EventMsg::ShutdownComplete)).await;
    serve_task
        .await
        .expect("serve 任务应正常结束")
        .expect("serve 应返回 Ok");
}

/// 回归:SessionConfigured / ShutdownComplete 同时走 turn 通道与 session
/// 扇出(见 submission_loop),serve 的两条转发路径各写过一遍导致 stdout
/// 重复。serve 侧已按事件类型去重 —— 本测试直接断言 serve_session 产出的
/// 事件流里两者各只出现一次。
#[tokio::test]
async fn serve_生命周期事件_不重复输出() {
    let thread = minimal_thread(vec![MockReply::Text { text: "ok".into() }]);
    let (mut w, mut rx, _serve) = start_serve(thread).await;

    send_sub(&mut w, &Submission::user_input("hi")).await;
    let mut session_configured = 0;
    let mut turn_complete = 0;
    loop {
        let ev = tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("等待事件超时")
            .expect("事件通道提前关闭");
        match &ev.msg {
            EventMsg::SessionConfigured(_) => session_configured += 1,
            EventMsg::TurnComplete(_) => {
                turn_complete += 1;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(session_configured, 1, "SessionConfigured 应只出现一次");
    assert_eq!(turn_complete, 1);

    send_sub(&mut w, &Submission::with_id("bye", Op::Shutdown)).await;
    let mut shutdown_complete = 0;
    loop {
        let ev = tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("等待事件超时")
            .expect("事件通道提前关闭");
        if matches!(ev.msg, EventMsg::ShutdownComplete) {
            shutdown_complete += 1;
            // Shutdown 后通道关闭:再 recv 一次确认没有第二条。
            let extra = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
            match extra {
                Ok(Some(e)) if matches!(e.msg, EventMsg::ShutdownComplete) => {
                    panic!("ShutdownComplete 不应重复输出")
                }
                _ => break,
            }
        }
    }
    assert_eq!(shutdown_complete, 1, "ShutdownComplete 应只出现一次");
}

#[tokio::test]
async fn serve_坏行_不终止会话() {
    let thread = minimal_thread(vec![MockReply::Text { text: "ok".into() }]);
    let (mut w, mut rx, _serve) = start_serve(thread).await;

    // 一行非法 JSON → Error 事件(code=invalid_submission),会话继续。
    w.write_all(b"this is not json\n").await.unwrap();
    w.flush().await.unwrap();
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::Error(_))).await;
    let EventMsg::Error(e) = msg else {
        unreachable!()
    };
    assert_eq!(e.code, "invalid_submission");

    // 随后的正常 submission 仍被处理。
    send_sub(&mut w, &Submission::user_input("还在吗")).await;
    expect_event(&mut rx, |m| matches!(m, EventMsg::TurnComplete(_))).await;
}

#[tokio::test]
async fn serve_空名注册_被拒绝() {
    let thread = minimal_thread(vec![]);
    let (mut w, mut rx, _serve) = start_serve(thread).await;

    // 空工具名 → tool_name_invalid 错误事件;正常名字被静默注册。
    let register = Submission::with_id(
        "reg-bad",
        Op::RegisterTools {
            tools: vec![RemoteToolSpec {
                name: "".into(),
                description: "无名字".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        },
    );
    send_sub(&mut w, &register).await;
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::Error(_))).await;
    let EventMsg::Error(e) = msg else {
        unreachable!()
    };
    assert_eq!(e.code, "tool_name_invalid");
}

#[tokio::test]
async fn serve_远程工具超时_以错误输出收尾() {
    // 脚本第一轮就要调工具,但客户端永不回执 → 远程工具超时。
    // 用极短的 env 超时让测试快速收敛。
    // SAFETY: 本测试二进制内独占使用该 env;serve_session 启动时读取。
    unsafe {
        std::env::set_var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS", "1");
    }
    let thread = minimal_thread(vec![
        MockReply::ToolCall {
            name: "slow".into(),
            args: serde_json::json!({}),
        },
        MockReply::Text {
            text: "超时后的收尾".into(),
        },
    ]);
    let (mut w, mut rx, _serve) = start_serve(thread).await;

    // 先注册 slow 工具,LLM 才能调到它。
    send_sub(
        &mut w,
        &Submission::with_id(
            "reg-slow",
            Op::RegisterTools {
                tools: vec![RemoteToolSpec {
                    name: "slow".into(),
                    description: "慢工具".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }],
            },
        ),
    )
    .await;
    send_sub(&mut w, &Submission::user_input("调一个慢工具")).await;
    let msg = expect_event(&mut rx, |m| matches!(m, EventMsg::ToolExecutionRequest(_))).await;
    let EventMsg::ToolExecutionRequest(req) = msg else {
        unreachable!()
    };
    // 不回执:等待 ToolCallEnd(is_error=true,内容含 timed out)。
    loop {
        let msg = expect_event(&mut rx, |m| {
            matches!(m, EventMsg::ToolCallEnd(_) | EventMsg::TurnComplete(_))
        })
        .await;
        if let EventMsg::ToolCallEnd(end) = &msg {
            assert!(end.is_error, "超时应产生错误输出");
            let text = serde_json::to_string(&end.output).unwrap();
            assert!(text.contains("timed out"), "got: {text}");
            break;
        }
        if matches!(msg, EventMsg::TurnComplete(_)) {
            panic!("turn 在 ToolCallEnd 之前完成,不合预期");
        }
    }
    let _ = req;
}

/// mock 的 stream() 能力健全性检查(防止 registry 侧接线退化)。
#[tokio::test]
async fn mock_client_在_registry_中可解析() {
    let registry = mock_registry(vec![MockReply::Text { text: "hi".into() }]);
    let next = registry
        .next_for("mock/mock-1", &[])
        .expect("mock pool 应可解析");
    let stream = next
        .client
        .stream(
            reflect_llm::ChatRequest::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("mock stream 不应失败");
    tokio::pin!(stream);
    let mut saw_stop = false;
    while let Some(ev) = futures::StreamExt::next(&mut stream).await {
        if matches!(ev.unwrap(), ChatEvent::MessageStop) {
            saw_stop = true;
        }
    }
    assert!(saw_stop);
}
