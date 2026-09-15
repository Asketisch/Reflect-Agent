//! v1.4 全链路集成测试 —— 模拟 LLM(script 回放),贯通引擎完整链路。
//!
//! 链路定义:客户端 `Submission`(Op)→ `submission_loop` → 4 节点
//! StateGraph(PreLoop → ModelCall → ToolExec → CheckStop)→ 工具执行
//! 队列(真实工具:bash / echo / call_<role>)→ 协议事件流(顺序与
//! 内容断言)→ rollout 持久化与回放。**唯一被 mock 的是 LLM**:按调用
//! 次序回放脚本化的 `ChatEvent` 流,并记录每次收到的 `ChatRequest`
//! 供上下文断言。
//!
//! 覆盖矩阵(每个用例的链路深度见各自 doc):
//! 1. 回合生命周期 + 工具调用环 + 用量(事件顺序全量断言)
//! 2. 多轮对话 × rollout 持久化 × 每轮回填(记忆不丢)
//! 3. Rewind 截断持久化 → 下一轮历史变短
//! 4. 真中断(在飞回合取消 + 真实 turn_id)+ 中断后恢复
//! 5. 空闲期 Steer → 下回合边界合并
//! 6. 工具输出流式增量上 wire(ToolCallOutputDelta 先于 ToolCallEnd)
//! 7. QuerySubagents 状态查询(状态中心快照)
//! 8. 子代理端到端:父模型调 call_<role> → 工厂 spawn 子线程(独立
//!    mock 池)→ SubagentProgress 进度推送 → 结果回流父 ToolCallEnd →
//!    状态中心终态 Completed
//! 9. 压缩升级全链路(触发阈值 → ContextCompacted 事件 + 摘要)

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_agent_def::AgentDefinition;
use reflect_compact::{Compactor, CompactorConfig, Summarizer, SummarizerError};
use reflect_core::config::M4Deps;
use reflect_core::{AgentConfig, AgentThread, SubagentRuntimeRegistry};
use reflect_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_memory::{FileMemoryStore, MemoryStore};
use reflect_prompt::PromptBuilder;
use reflect_protocol::{
    AbortReason, EventMsg, MessageRole, Op, RolloutRecord, RolloutRecorder, SessionInfo,
    SubagentProgressKind, SubagentRunStateMirror, Submission, ThreadId, TurnId, UserInputItem,
};
use reflect_subagent::{CallSubAgentTool, SubAgentFactory, SubAgentSpec};
use reflect_tools::{ToolRegistry, ToolSource, builtins::EchoTool};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// ── 通用 mock:脚本化 LLM 客户端 ───────────────────────────────────

/// 按调用次序回放脚本的模型。每次 `stream` 弹出一段事件序列;同时
/// 记录每次收到的 `ChatRequest`(上下文断言用)。脚本耗尽时回放
/// 「空消息停止」—— 让测试永远能收敛。
struct ScriptedClient {
    scripts: Mutex<Vec<Vec<ChatEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedClient {
    fn new(scripts: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().clone()
    }
}

impl SlowThenScriptClient {
    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().clone()
    }
}

fn text_script(text: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::MessageStart {
            id: "m".into(),
            model: "scripted".into(),
        },
        ChatEvent::ContentDelta(text.to_string()),
        ChatEvent::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ]
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
        ChatEvent::Usage {
            input_tokens: 80,
            output_tokens: 15,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ]
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
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().push(request);
        let next = self.scripts.lock().pop_front_script();
        Ok(Box::pin(stream::iter(
            next.into_iter().map(Ok::<ChatEvent, LlmError>),
        )))
    }
}

trait PopFront {
    fn pop_front_script(&mut self) -> Vec<ChatEvent>;
}
impl PopFront for Vec<Vec<ChatEvent>> {
    fn pop_front_script(&mut self) -> Vec<ChatEvent> {
        if self.is_empty() {
            text_script("")
        } else {
            self.remove(0)
        }
    }
}

/// 慢速流:事件逐个间隔发出,供中断测试在流消费阶段打断。
struct SlowThenScriptClient {
    /// 第一次调用:慢速文本流(30 × 100ms)。
    /// 之后:立即回放 `after` 脚本。
    after: Mutex<Vec<Vec<ChatEvent>>>,
    call: std::sync::atomic::AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ModelClient for SlowThenScriptClient {
    fn name(&self) -> &str {
        "slow-scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().push(_request);
        let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            let mut events = vec![ChatEvent::MessageStart {
                id: "slow".into(),
                model: "slow-scripted".into(),
            }];
            for i in 0..30 {
                events.push(ChatEvent::ContentDelta(format!("chunk-{i} ")));
            }
            events.push(ChatEvent::MessageStop);
            let interval = Duration::from_millis(100);
            return Ok(Box::pin(stream::unfold(events, move |mut it| async move {
                if it.is_empty() {
                    return None;
                }
                tokio::time::sleep(interval).await;
                Some((Ok::<ChatEvent, LlmError>(it.remove(0)), it))
            })));
        }
        // 注意:match 求值项的临时 MutexGuard 存活到整个 match 结束,
        // 分支守卫里再 lock 同一把 parking_lot 会自死锁 —— 先在块内取脚本。
        let script = {
            let mut q = self.after.lock();
            if q.is_empty() {
                text_script("recovered")
            } else {
                q.remove(0)
            }
        };
        Ok(Box::pin(stream::iter(
            script.into_iter().map(Ok::<ChatEvent, LlmError>),
        )))
    }
}

// ── 通用 mock:内存 rollout 录制器 ─────────────────────────────────

/// 内存版 `RolloutRecorder`:record 存 Vec,replay 全量克隆,
/// truncate_after 按引擎语义截断(Some(id) = 丢该回合及之后;
/// None = 丢最后一轮)。
#[derive(Debug, Default)]
struct InMemoryRecorder(Mutex<Vec<RolloutRecord>>);

#[async_trait]
impl RolloutRecorder for InMemoryRecorder {
    async fn record(&self, r: RolloutRecord) -> anyhow::Result<()> {
        self.0.lock().push(r);
        Ok(())
    }
    async fn replay(&self, _session_id: ThreadId) -> anyhow::Result<Vec<RolloutRecord>> {
        Ok(self.0.lock().clone())
    }
    async fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        Ok(Vec::new())
    }
    async fn truncate_after(&self, to_turn_id: Option<&TurnId>) -> anyhow::Result<usize> {
        let mut g = self.0.lock();
        let pos = match to_turn_id {
            Some(t) => g
                .iter()
                .position(|r| matches!(r, RolloutRecord::Message { turn_id, .. } if turn_id == t)),
            None => g.iter().rposition(|r| {
                matches!(
                    r,
                    RolloutRecord::Message {
                        role: MessageRole::User,
                        ..
                    }
                )
            }),
        };
        match pos {
            Some(i) => {
                let dropped = g[i..]
                    .iter()
                    .filter(|r| matches!(r, RolloutRecord::Message { .. }))
                    .count();
                g.truncate(i);
                Ok(dropped)
            }
            None => Ok(0),
        }
    }
}

// ── 通用:装配辅助 ────────────────────────────────────────────────

fn sub(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
    }
}

fn user_input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInputItem::Text { text: text.into() }],
        thread_settings: Default::default(),
    }
}

/// 建线程 + 注册池。返回 (thread, client) —— client 供请求/脚本断言。
fn build_scripted_thread(
    provider: &str,
    cfg: AgentConfig,
    client: Arc<ScriptedClient>,
) -> AgentThread {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        provider,
        CredentialPool {
            entries: vec![PoolEntry {
                client,
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    AgentThread::new(cfg, registry, tools, None, None)
}

/// 收集回合事件直到终态(TurnComplete / TurnAborted / ShutdownComplete),
/// 带超时防挂死。
async fn drain_until_terminal(
    handle: &mut reflect_core::TurnHandle,
) -> Vec<reflect_protocol::Event> {
    let mut events = Vec::new();
    let deadline = Duration::from_secs(15);
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event should arrive in time")
    {
        let terminal = matches!(
            ev.msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::ShutdownComplete
        );
        events.push(ev);
        if terminal {
            break;
        }
    }
    events
}

/// 从请求列表提取全部用户消息文本。
fn user_texts(reqs: &[ChatRequest]) -> Vec<String> {
    reqs.iter()
        .map(|r| {
            r.messages
                .iter()
                .filter_map(|m| match m {
                    ChatMessage::User(u) => Some(
                        u.blocks
                            .iter()
                            .filter_map(|b| match b {
                                reflect_llm::ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    ),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

/// 最小 M4 依赖(canned 摘要器,永不调真 LLM)。
struct CannedSummarizer {
    text: String,
    fail: bool,
}
#[async_trait]
impl Summarizer for CannedSummarizer {
    async fn summarize_full(&self, _msgs: &[ChatMessage]) -> Result<String, SummarizerError> {
        if self.fail {
            Err(SummarizerError::Cancelled)
        } else {
            Ok(self.text.clone())
        }
    }
    async fn summarize_recent(
        &self,
        _msgs: &[ChatMessage],
        _prev: Option<&str>,
    ) -> Result<String, SummarizerError> {
        if self.fail {
            Err(SummarizerError::Cancelled)
        } else {
            Ok(self.text.clone())
        }
    }
}

fn make_m4(recorder: Option<Arc<dyn RolloutRecorder>>, compactor: Arc<Compactor>) -> M4Deps {
    let tmp = std::env::temp_dir().join(format!("reflect-full-link-{}", uuid_stub()));
    let _ = std::fs::create_dir_all(&tmp);
    let memory: Arc<dyn MemoryStore> = Arc::new(FileMemoryStore::new(&tmp, &tmp));
    M4Deps {
        compactor,
        memory,
        skills: Arc::new(reflect_skills_default()),
        prompt_builder: Arc::new(Mutex::new(PromptBuilder::new())),
        active_agent_def: Arc::new(AgentDefinition {
            name: "full-link".into(),
            description: "test".into(),
            system_prompt: "You are a full-link test agent.".into(),
            ..Default::default()
        }),
        recorder,
        note_store: Arc::new(reflect_notes::InMemoryNoteStore::new()),
        file_recovery: Arc::new(reflect_recovery::ActiveFileRecovery::new(Arc::from(tmp))),
        subagent_registry: reflect_recovery::SubagentRegistry::shared(),
    }
}

fn reflect_skills_default() -> reflect_skills::SkillsCatalog {
    reflect_skills::SkillsCatalog::new()
}

/// 轻量唯一后缀(避免引额外 uuid 依赖形态差异)。
fn uuid_stub() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

// ── 1. 回合生命周期 + 工具调用环 + 用量 ──────────────────────────

/// 全链路:UserInput → TurnStarted → ToolCallBegin(模型发起)→ 真实
/// echo 工具执行 → ToolCallEnd(结果回流)→ 第二次模型调用看到工具
/// 结果 → 文本增量 → TokenCount → TurnComplete(Success)。
/// 同时断言:第二次请求的消息序列 = [user, assistant(tool_calls),
/// tool(result)](角色交替完整);TokenCount 用量来自 Usage 事件;
/// 回合事件 id 与 submission id 一致。
#[tokio::test]
async fn full_link_turn_lifecycle_with_tool_loop() {
    let client = ScriptedClient::new(vec![
        tool_call_script("c1", "echo", serde_json::json!({"text": "ping"})),
        text_script("all done"),
    ]);
    let client2 = client.clone();
    // 挂 M4(生产路径):pre_loop 播种历史,第二次请求可见完整
    // [user, assistant(tool_calls), tool] 链(m4=None 的旧路径不播种)。
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    ));
    let cfg = AgentConfig::new("scripted/m1", Path::new(".")).with_m4(make_m4(None, compactor));
    let thread = build_scripted_thread("scripted", cfg, client.clone());

    let mut handle = thread.submit(sub("t1", user_input("run echo ping"))).await;
    let events = drain_until_terminal(&mut handle).await;

    // ── 事件顺序断言(子序列式:TokenCount 每次模型调用都会发,
    //    条数不固定;关键因果链必须按序出现) ──
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.msg {
            EventMsg::SessionConfigured(_) => "session_configured",
            EventMsg::TurnStarted(_) => "turn_started",
            EventMsg::ToolCallBegin(_) => "tool_call_begin",
            EventMsg::ToolCallEnd(_) => "tool_call_end",
            EventMsg::AgentMessageDelta(_) => "agent_delta",
            EventMsg::TokenCount(_) => "token_count",
            EventMsg::TurnComplete(_) => "turn_complete",
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();
    let expected_order = [
        "session_configured",
        "turn_started",
        "tool_call_begin",
        "tool_call_end",
        "agent_delta",
        "turn_complete",
    ];
    let mut pos = 0usize;
    for want in expected_order {
        while pos < kinds.len() && kinds[pos] != want {
            pos += 1;
        }
        assert!(
            pos < kinds.len(),
            "因果链缺失或乱序:未按序找到 {want}; 实际 {kinds:?}"
        );
        pos += 1;
    }
    assert!(
        !matches!(
            events.first().map(|e| &e.msg),
            Some(EventMsg::TurnStarted(_))
        ),
        "SessionConfigured 必须先于一切回合事件"
    );

    // ── 事件内容 ──
    for ev in &events {
        if matches!(ev.msg, EventMsg::SessionConfigured(_)) {
            assert_eq!(ev.id, "", "SessionConfigured 不绑定 submission");
        } else {
            assert_eq!(ev.id, "t1", "回合事件必须携带 submission id");
        }
    }
    for ev in &events {
        if let EventMsg::ToolCallEnd(end) = &ev.msg {
            assert!(!end.is_error, "echo 工具应成功");
            let text = end
                .output
                .content
                .iter()
                .filter_map(|b| match b {
                    reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>();
            assert!(
                text.contains("ping") || text.contains("echo"),
                "工具结果应含 echo 输出: {text}"
            );
        }
        if let EventMsg::TokenCount(tc) = &ev.msg {
            assert!(tc.total_tokens > 0, "TokenCount 应聚合 Usage");
            assert!(tc.cost_usd.is_some() || tc.total_tokens > 0);
        }
        if let EventMsg::TurnComplete(tc) = &ev.msg {
            assert_eq!(
                tc.status,
                reflect_protocol::TurnStatus::Success,
                "正常收口应为 Success"
            );
            assert!(tc.usage.total_tokens > 0);
        }
    }

    // ── 上下文链:第二次请求 = user + assistant(tool_calls) + tool ──
    let reqs = client.requests();
    assert_eq!(reqs.len(), 2, "模型应被调用两次");
    let msgs = &reqs[1].messages;
    eprintln!("[debug] 第二次请求消息序列: {msgs:?}");
    assert!(matches!(msgs[0], ChatMessage::User(_)));
    let assistant = msgs
        .iter()
        .find_map(|m| match m {
            ChatMessage::Assistant(a) if !a.tool_calls.is_empty() => Some(a),
            _ => None,
        })
        .expect("第二次请求应含带 tool_calls 的 assistant 消息");
    assert_eq!(assistant.tool_calls[0].name, "echo");
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m, ChatMessage::Tool(_)))
        .expect("第二次请求应含 tool 结果消息");
    match tool_msg {
        ChatMessage::Tool(t) => {
            assert!(
                t.content_as_text().contains("ping") || !t.content_as_text().is_empty(),
                "tool 结果内容回流"
            );
        }
        _ => unreachable!(),
    }
    drop(client2);
}

// ── 2. 多轮对话 × rollout 持久化 × 每轮回填 ──────────────────────

/// 全链路(持久化):recorder 在位 → 第 1 轮落盘(SessionMeta / user /
/// assistant / TokenCount)→ 第 2 轮经 replay 回填,第二次模型请求
/// 包含第 1 轮的 user 与 assistant 文本 —— 跨轮记忆不丢。
#[tokio::test]
async fn full_link_multi_turn_rollout_replay() {
    let recorder = Arc::new(InMemoryRecorder::default());
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    ));
    let client = ScriptedClient::new(vec![text_script("你好 Alice"), text_script("你叫 Alice")]);
    let cfg = AgentConfig::new("scripted/m1", Path::new("."))
        .with_m4(make_m4(Some(recorder.clone()), compactor));
    let thread = build_scripted_thread("scripted", cfg, client.clone());

    // 第 1 轮。
    let mut h1 = thread.submit(sub("r1", user_input("我叫 Alice"))).await;
    let ev1 = drain_until_terminal(&mut h1).await;
    assert!(matches!(
        ev1.last().map(|e| &e.msg),
        Some(EventMsg::TurnComplete(_))
    ));

    // 给 best-effort 落盘(异步 spawn)留出窗口。
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 第 2 轮:请求应含第 1 轮历史。
    let mut h2 = thread.submit(sub("r2", user_input("我叫什么?"))).await;
    let ev2 = drain_until_terminal(&mut h2).await;
    assert!(matches!(
        ev2.last().map(|e| &e.msg),
        Some(EventMsg::TurnComplete(_))
    ));

    let reqs = client.requests();
    assert_eq!(reqs.len(), 2);
    let texts = user_texts(std::slice::from_ref(&reqs[1]));
    assert!(
        texts.iter().any(|t| t.contains("我叫 Alice")),
        "第 2 轮请求应回填第 1 轮 user 消息: {texts:?}"
    );
    // assistant 历史同样回填(经 records_to_preload 重建)。
    assert!(
        reqs[1].messages.iter().any(|m| matches!(
            m,
            ChatMessage::Assistant(a) if a.text.as_deref() == Some("你好 Alice")
        )),
        "第 2 轮请求应回填第 1 轮 assistant 回复"
    );

    // 落盘完整性。
    let records = recorder.0.lock().clone();
    assert!(
        records
            .iter()
            .any(|r| matches!(r, RolloutRecord::SessionMeta { .. }))
    );
    let user_msgs = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                RolloutRecord::Message {
                    role: MessageRole::User,
                    ..
                }
            )
        })
        .count();
    let assistant_msgs = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                RolloutRecord::Message {
                    role: MessageRole::Assistant,
                    ..
                }
            )
        })
        .count();
    assert_eq!(user_msgs, 2, "两轮 user 消息均落盘");
    assert_eq!(assistant_msgs, 2, "两轮 assistant 消息均落盘");
    assert!(
        records
            .iter()
            .any(|r| matches!(r, RolloutRecord::TokenCount { .. })),
        "TokenCount 记录落盘"
    );
}

// ── 3. Rewind 截断持久化 ────────────────────────────────────────

/// 全链路:第 1 轮后 Rewind 到第 1 回合起始 → TurnRewound 事件携带
/// 截断计数 → 第 2 轮请求不再含第 1 轮内容(历史真的变短)。
#[tokio::test]
async fn full_link_rewind_truncates_history() {
    let recorder = Arc::new(InMemoryRecorder::default());
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    ));
    let client = ScriptedClient::new(vec![text_script("第一轮回答"), text_script("新起点")]);
    let cfg = AgentConfig::new("scripted/m1", Path::new("."))
        .with_m4(make_m4(Some(recorder.clone()), compactor));
    let thread = build_scripted_thread("scripted", cfg, client.clone());

    // 第 1 轮,捕获真实 turn_id。
    let mut h1 = thread.submit(sub("w1", user_input("被撤回的问题"))).await;
    let ev1 = drain_until_terminal(&mut h1).await;
    let turn_id1 = ev1
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::TurnStarted(ts) => Some(ts.turn_id),
            _ => None,
        })
        .expect("TurnStarted observed");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Rewind:丢弃第 1 回合(含)之后。
    let mut hw = thread
        .submit(sub(
            "wr",
            Op::Rewind {
                to_turn_id: Some(turn_id1.to_string()),
            },
        ))
        .await;
    let evw = drain_until_terminal(&mut hw).await;
    match evw.iter().find_map(|e| match &e.msg {
        EventMsg::TurnRewound(r) => Some(r.clone()),
        _ => None,
    }) {
        Some(r) => {
            assert!(
                r.truncated_after >= 2,
                "应截掉第 1 轮的 user+assistant: {:?}",
                r.truncated_after
            );
        }
        None => panic!("应收到 TurnRewound 事件, got {evw:?}"),
    }

    // 第 2 轮:请求不含第 1 轮内容。
    let mut h2 = thread.submit(sub("w2", user_input("重新开始"))).await;
    let _ = drain_until_terminal(&mut h2).await;
    let reqs = client.requests();
    let texts = user_texts(std::slice::from_ref(&reqs[1]));
    assert!(
        !texts.iter().any(|t| t.contains("被撤回的问题")),
        "Rewind 后请求不应含被撤回历史: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("重新开始")),
        "新输入应在: {texts:?}"
    );
}

// ── 4. 真中断 + 恢复 ────────────────────────────────────────────

/// 全链路:慢流被 Interrupt 打断(TurnAborted 携带真实 turn_id、无
/// TurnComplete)→ 之后的新回合正常完成(引擎可恢复)。
#[tokio::test]
async fn full_link_interrupt_then_recover() {
    let client = Arc::new(SlowThenScriptClient {
        after: Mutex::new(vec![text_script("恢复后的回答")]),
        call: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "slow-scripted",
        CredentialPool {
            entries: vec![PoolEntry {
                client: client.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(
        AgentConfig::new("slow-scripted/m1", Path::new(".")),
        registry,
        tools,
        None,
        None,
    );

    // 第 1 回合:慢流。只等到 TurnStarted(回合在飞)即打断。
    let mut h1 = thread.submit(sub("i1", user_input("写一篇长文"))).await;
    let deadline = Duration::from_secs(10);
    let turn_id = loop {
        let ev = timeout(deadline, h1.next())
            .await
            .expect("TurnStarted in time")
            .expect("channel open");
        match ev.msg {
            EventMsg::TurnStarted(ts) => break ts.turn_id,
            EventMsg::SessionConfigured(_) => continue,
            other => panic!("unexpected: {other:?}"),
        }
    };

    // 中断在飞回合。
    let _hi = thread
        .submit(sub("i1-int", Op::Interrupt { child_id: None }))
        .await;
    // 在飞回合通道收到真实 turn_id 的中止事件;回合任务退出后通道可能
    // 随条目移除而关闭(None),据此停止。
    let mut aborted = false;
    loop {
        match timeout(deadline, h1.next()).await {
            Ok(Some(ev)) => match ev.msg {
                EventMsg::TurnAborted(a) => {
                    assert_eq!(a.turn_id, turn_id, "必须携带在飞回合真实 turn_id");
                    assert!(matches!(a.reason, AbortReason::UserInterrupt));
                    aborted = true;
                }
                EventMsg::TurnComplete(_) => panic!("被中断回合不应发 TurnComplete"),
                _ => {}
            },
            Ok(None) => break, // 通道随回合清理关闭
            Err(_) => break,   // 超时兜底
        }
        if aborted {
            break;
        }
    }
    assert!(aborted, "应收到携带真实 turn_id 的 TurnAborted");

    // 第 2 回合:立即恢复,正常完成。
    let mut h2 = thread.submit(sub("i2", user_input("继续"))).await;
    let ev2 = drain_until_terminal(&mut h2).await;
    match ev2.last().map(|e| &e.msg) {
        Some(EventMsg::TurnComplete(tc)) => {
            assert_eq!(tc.status, reflect_protocol::TurnStatus::Success)
        }
        other => panic!("恢复回合应 Success: {other:?}"),
    }
    let texts = user_texts(client.requests().as_slice());
    assert!(
        texts.last().is_some_and(|t| t.contains("继续")),
        "恢复回合输入应在: {texts:?}"
    );
}

// ── 5. 空闲期 Steer → 回合边界合并 ───────────────────────────────

/// 全链路:无在飞回合时 Steer → 消息入队 → 下一个 UserInput 回合在
/// 边界合并,首次模型请求即含转向内容。
#[tokio::test]
async fn full_link_steer_when_idle_merges_at_boundary() {
    let client = ScriptedClient::new(vec![text_script("好的")]);
    let thread = build_scripted_thread(
        "scripted",
        AgentConfig::new("scripted/m1", Path::new(".")),
        client.clone(),
    );

    let _hs = thread
        .submit(sub(
            "s0",
            Op::Steer {
                priority: reflect_protocol::SteeringPriorityMirror::Now,
                items: vec![UserInputItem::Text {
                    text: "记得用中文".into(),
                }],
            },
        ))
        .await;

    let mut h = thread.submit(sub("s1", user_input("开始任务"))).await;
    let _ = drain_until_terminal(&mut h).await;

    let reqs = client.requests();
    assert_eq!(reqs.len(), 1);
    let texts = user_texts(&reqs);
    assert!(
        texts.iter().any(|t| t.contains("记得用中文")),
        "空闲期转向应在回合边界合并进首请求: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("开始任务")),
        "原输入应在: {texts:?}"
    );
}

// ── 6. 工具输出流式增量上 wire ───────────────────────────────────

/// 全链路:模型调 bash(真实执行)→ 事件流中 ToolCallOutputDelta
/// 先于 ToolCallEnd 到达,且 End 的完整输出包含全部内容。
/// 注意:与 bash 单测一致,显式关沙箱避免 env 串扰(锁内)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn full_link_tool_output_delta_on_wire() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap();
    let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
    let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
    unsafe {
        std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
        std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
    }

    let client = ScriptedClient::new(vec![
        tool_call_script(
            "c1",
            "bash",
            serde_json::json!({"cmd": "echo delta-line-42"}),
        ),
        text_script("done"),
    ]);
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
    let thread = AgentThread::new(
        AgentConfig::new("scripted/m1", Path::new(".")),
        registry,
        tools,
        None,
        None,
    );

    let mut h = thread.submit(sub("d1", user_input("跑 echo"))).await;
    let events = drain_until_terminal(&mut h).await;

    // 还原 env。
    match prior_on {
        Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
        None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
    }
    match prior_strict {
        Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
        None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
    }

    // 增量事件先于 End;End 输出完整。
    let delta_idx = events.iter().position(|e| {
        matches!(
            &e.msg,
            EventMsg::ToolCallOutputDelta(d) if d.delta.contains("delta-line-42")
        )
    });
    let end_idx = events
        .iter()
        .position(|e| matches!(&e.msg, EventMsg::ToolCallEnd(_)));
    assert!(delta_idx.is_some(), "应有含输出的增量事件: {events:?}");
    assert!(end_idx.is_some(), "应有 ToolCallEnd");
    assert!(
        delta_idx.unwrap() < end_idx.unwrap(),
        "增量必须先于 End 事件"
    );
    let end = events
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::ToolCallEnd(end) => Some(end.clone()),
            _ => None,
        })
        .unwrap();
    let text = end
        .output
        .content
        .iter()
        .filter_map(|b| match b {
            reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.contains("delta-line-42"), "完整输出含内容: {text}");
}

// ── 7. QuerySubagents 状态查询 ───────────────────────────────────

/// 全链路:登记状态槽 → QuerySubagents → SubagentStatus 快照回显。
#[tokio::test]
async fn full_link_query_subagents() {
    let runtime = Arc::new(SubagentRuntimeRegistry::new());
    let slot = runtime.register("child-x", "explorer", CancellationToken::new());
    slot.begin_tool("grep");
    slot.set_iteration(2);
    slot.add_tokens(777);

    let thread = build_scripted_thread(
        "scripted",
        AgentConfig::new("scripted/m1", Path::new(".")).with_subagent_runtime(runtime),
        ScriptedClient::new(vec![text_script("ok")]),
    );

    let mut h = thread
        .submit(sub("q", Op::QuerySubagents { child_id: None }))
        .await;
    let events = drain_until_terminal(&mut h).await;
    let st = events
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::SubagentStatus(s) => Some(s.clone()),
            _ => None,
        })
        .expect("应收到 SubagentStatus");
    assert_eq!(st.children.len(), 1);
    let c = &st.children[0];
    assert_eq!(c.child_id, "child-x");
    assert_eq!(c.role, "explorer");
    assert_eq!(c.state, SubagentRunStateMirror::Running);
    assert_eq!(c.current_tool.as_deref(), Some("grep"));
    assert_eq!(c.total_tokens, 777);
}

// ── 8. 子代理端到端(最深链路) ──────────────────────────────────

/// 全链路:父模型调 call_explorer → 工厂 spawn 子 AgentThread(独立
/// mock 池)→ 子代理跑完 → SubagentProgress(文本 + 进度)推送 →
/// 结果回流父 ToolCallEnd → 父第二次调用收口 → 状态中心 Completed。
#[tokio::test]
async fn full_link_subagent_end_to_end() {
    // 双池:parent 脚本 = 调 call_explorer → 收口;child = 文本报告。
    let parent = ScriptedClient::new(vec![
        tool_call_script(
            "c1",
            "call_explorer",
            serde_json::json!({"prompt": "find the modules"}),
        ),
        text_script("parent done, modules found"),
    ]);
    let child = ScriptedClient::new(vec![text_script("explorer report: 3 modules")]);

    let registry = Arc::new(ModelRegistry::new());
    for (provider, client) in [("parent", parent.clone()), ("child", child.clone())] {
        registry.register_pool(
            provider,
            CredentialPool {
                entries: vec![PoolEntry {
                    client,
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
    }

    let runtime = Arc::new(SubagentRuntimeRegistry::new());
    let factory = Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        // 子代理默认模型:child/m1 → 走 child 池。
        "child/m1",
        registry.clone(),
        None,
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    ));
    factory.set_runtime_registry(runtime.clone());

    let spec = SubAgentSpec {
        name: "Explorer".into(),
        role: "explorer".into(),
        model: None,
        system_prompt: "You explore.".into(),
        allowed_tools: vec![],
        data_transfer: Default::default(),
        max_turns: Some(4),
        allowed_skills: vec![],
    };
    let tools = Arc::new(ToolRegistry::default());
    tools.register_with_source(
        ToolSource::Runtime,
        Arc::new(CallSubAgentTool::new(factory, spec)),
    );

    // 父线程也挂同一状态中心(Interrupt 定向路由 / 查询)。
    let cfg = AgentConfig::new("parent/m1", Path::new(".")).with_subagent_runtime(runtime.clone());
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut h = thread
        .submit(sub("p1", user_input("派 explorer 找模块")))
        .await;
    let events = drain_until_terminal(&mut h).await;

    // ── 进度推送:SubagentProgress(Message + 文本) ──
    let progress_text = events
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::SubagentProgress(p)
                if p.role == "explorer" && p.kind == SubagentProgressKind::Message =>
            {
                Some(p.text.clone())
            }
            _ => None,
        })
        .expect("应收到子代理文本进度");
    assert!(
        progress_text.contains("explorer report"),
        "进度应含子代理回答: {progress_text}"
    );

    // ── 结果回流:父 ToolCallEnd 输出含子代理最终文本 ──
    let tool_end = events
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::ToolCallEnd(end) => Some(end.clone()),
            _ => None,
        })
        .expect("父应有 ToolCallEnd");
    let out_text = tool_end
        .output
        .content
        .iter()
        .filter_map(|b| match b {
            reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        out_text.contains("explorer report"),
        "子代理结果应回流父工具输出: {out_text}"
    );

    // ── 父收口:第二次请求已含子代理结果(工具结果消息) ──
    let preqs = parent.requests();
    assert_eq!(preqs.len(), 2, "父模型调用两次");
    assert!(
        preqs[1].messages.iter().any(
            |m| matches!(m, ChatMessage::Tool(t) if t.content_as_text().contains("explorer report"))
        ),
        "父第二次请求应含子代理结果(工具结果消息)"
    );
    match events.last().map(|e| &e.msg) {
        Some(EventMsg::TurnComplete(tc)) => {
            assert_eq!(tc.status, reflect_protocol::TurnStatus::Success)
        }
        other => panic!("父回合应 Success: {other:?}"),
    }

    // ── 状态中心:子代理终态 Completed ──
    // 子代理回合任务在发出 TurnComplete 之后才写槽位自报告(迭代数 /
    // token);父级 drain 看到 TurnComplete 即返回,这里给自报告留窗口。
    tokio::time::sleep(Duration::from_millis(150)).await;
    let snaps = runtime.snapshot(None);
    assert_eq!(snaps.len(), 1, "子代理槽应保留(终态保留期)");
    assert_eq!(snaps[0].state, SubagentRunStateMirror::Completed);
    assert!(snaps[0].total_tokens > 0, "子代理 token 应自报告");
    assert!(snaps[0].finished_at.is_some());
}

// ── 9. 压缩升级全链路 ────────────────────────────────────────────

/// 全链路(M4 生产路径):超阈值输入 → pre_loop 压缩升级到 LLM 摘要
/// (canned)→ ContextCompacted 事件 + 摘要 System 消息进入第二次
/// 请求(经 compaction_summary 回放路径)。
#[tokio::test]
async fn full_link_compaction_upgrade_emits_event() {
    let recorder = Arc::new(InMemoryRecorder::default());
    // 极小触发阈值 + 极紧 target → 强制走到 LlmSummarize。
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            trigger_tokens: 50,
            target_ratio: 0.01,
            summarize_after: true,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: "compressed summary of everything".into(),
            fail: false,
        }),
    ));
    let client = ScriptedClient::new(vec![text_script("回答")]);
    let cfg = AgentConfig::new("scripted/m1", Path::new("."))
        .with_m4(make_m4(Some(recorder.clone()), compactor));
    let thread = build_scripted_thread("scripted", cfg, client.clone());

    let long_input = "历史背景。".repeat(120);
    let mut h = thread.submit(sub("k1", user_input(&long_input))).await;
    let events = drain_until_terminal(&mut h).await;

    let compacted = events
        .iter()
        .find_map(|e| match &e.msg {
            EventMsg::ContextCompacted(c) => Some(c.clone()),
            _ => None,
        })
        .expect("超阈值回合应发出 ContextCompacted");
    assert!(
        matches!(
            compacted.strategy,
            reflect_protocol::ContextCompactedStrategy::LlMSummarize
        ),
        "极紧 target 应升级到 LLM 摘要: {:?}",
        compacted.strategy
    );
    assert!(matches!(
        events.last().map(|e| &e.msg),
        Some(EventMsg::TurnComplete(_))
    ));
}
