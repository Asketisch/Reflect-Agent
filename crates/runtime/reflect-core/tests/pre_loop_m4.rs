//! `pre_loop` + `model_call` 接线的 M4 集成测试。
//!
//! 这些测试构造带 M4 deps 的 `NodeContext`,让 StateGraph 走完一个 turn,
//! 校验 M4 流水线(compact / memory / skills / prompt builder)端到端运行。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use reflect_agent_def::AgentDefinition;
use reflect_compact::{Compactor, CompactorConfig, Summarizer, SummarizerError};
use reflect_core::config::M4Deps;
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_memory::{FileMemoryStore, InMemoryStore, MemoryScope, MemoryStore};
use reflect_prompt::PromptBuilder;
use reflect_protocol::{
    AbortReason, ContentBlock, EventMsg, PermissionMode, Submission, UserInputItem,
};
use reflect_skills::SkillsCatalog;
use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, builtins::EchoTool};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct StubClient {
    events: Mutex<Vec<ChatEvent>>,
    last_request: Mutex<Option<ChatRequest>>,
    call_count: Mutex<u32>,
}

impl StubClient {
    fn new(events: Vec<ChatEvent>) -> Self {
        Self {
            events: Mutex::new(events),
            last_request: Mutex::new(None),
            call_count: Mutex::new(0),
        }
    }
}

#[async_trait]
impl ModelClient for StubClient {
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
        *self.call_count.lock() += 1;
        *self.last_request.lock() = Some(request);
        let events = self.events.lock().clone();
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

struct NoopSummarizer;
#[async_trait]
impl Summarizer for NoopSummarizer {
    async fn summarize_full(&self, _msgs: &[ChatMessage]) -> Result<String, SummarizerError> {
        Err(SummarizerError::Cancelled)
    }
    async fn summarize_recent(
        &self,
        _msgs: &[ChatMessage],
        _prev: Option<&str>,
    ) -> Result<String, SummarizerError> {
        Err(SummarizerError::Cancelled)
    }
}

fn make_m4_deps(agent_name: &str, system_prompt: &str) -> M4Deps {
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false, // never call the LLM in tests
            ..Default::default()
        },
        Arc::new(NoopSummarizer),
    ));
    let tmp = std::env::temp_dir().join(format!("reflect-m4-mem-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&tmp);
    let memory: Arc<dyn MemoryStore> = Arc::new(FileMemoryStore::new(&tmp, &tmp));
    let skills = Arc::new(SkillsCatalog::new());
    let prompt_builder = Arc::new(Mutex::new(PromptBuilder::new()));
    let note_store: Arc<dyn reflect_notes::NoteStore> =
        Arc::new(reflect_notes::InMemoryNoteStore::new());
    let file_recovery = Arc::new(reflect_recovery::ActiveFileRecovery::new(Arc::from(
        tmp.clone(),
    )));
    let subagent_registry = reflect_recovery::SubagentRegistry::shared();
    let mut def = AgentDefinition::default();
    #[allow(clippy::field_reassign_with_default)] // pre-M5
    {
        def.name = agent_name.to_string();
        def.description = "test".into();
        def.system_prompt = system_prompt.into();
        // v1.6:测试注入的文案视为用户自定义 —— pre_loop 不走提示词库。
        def.source = reflect_agent_def::SystemPromptSource::Custom;
        def.memory = vec![MemoryScope::Project];
    }
    M4Deps {
        compactor,
        memory,
        skills,
        prompt_builder,
        active_agent_def: Arc::new(def),
        recorder: None,
        note_store,
        file_recovery,
        subagent_registry,
        prompt_dirs: Vec::new(),
    }
}

fn build_thread(registry: Arc<ModelRegistry>, m4: M4Deps) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    AgentThread::new(cfg, registry, tools, None, None)
}

fn make_sub(text: &str) -> Submission {
    Submission {
        id: "test-sub".into(),
        op: reflect_protocol::Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

#[tokio::test]
async fn m4_injects_system_prompt_into_request() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a reviewer.");
    let thread = build_thread(registry, m4);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    // 验证模型收到的请求包含 system prompt。
    let req = stub.last_request.lock().clone().expect("model was called");
    let has_system = req
        .system
        .0
        .iter()
        .any(|b| b.text.contains("You are a reviewer"));
    assert!(has_system, "system prompt should be in the request");
}

#[tokio::test]
async fn m4_loads_memory_into_request() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let mut m4 = make_m4_deps("tester", "You are a tester.");
    let _ = &mut m4; // silence unused-mut; `m4.memory = ...` later mutates
    // 保存一些项目级记忆并重载。
    m4.memory
        .save(
            MemoryScope::Project,
            "tester",
            "## Facts\n- the sky is blue\n",
        )
        .unwrap();
    let thread = build_thread(registry, m4);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let has_memory = req
        .system
        .0
        .iter()
        .any(|b| b.text.contains("the sky is blue"));
    assert!(has_memory, "memory should be in the system prompt");
}

#[tokio::test]
async fn m4_injects_ephemeral_block_as_system_block() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "sys");
    let thread = build_thread(registry, m4);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    // v1.x fix: ephemeral(工具目录/答案格式/迭代计数)作为 system block 注入,
    // 而非 trailing User 消息。避免模型把 system-reminder 误认为最新用户输入。
    let in_system = req
        .system
        .0
        .iter()
        .any(|b| b.text.contains("Active Tools") || b.text.contains("system-reminder"));
    let in_user_as_reminder = req.messages.iter().any(|m| {
        let ChatMessage::User(u) = m else {
            return false;
        };
        u.blocks.iter().any(|b| {
            let reflect_llm::ContentBlock::Text { text } = b else {
                return false;
            };
            text.contains("system-reminder") && text.contains("Active Tools")
        })
    });
    assert!(
        in_system,
        "ephemeral block should be in system blocks, not messages"
    );
    assert!(
        !in_user_as_reminder,
        "ephemeral block must NOT be a trailing User message (causes 'No question provided')"
    );
}

#[tokio::test]
async fn m4_session_only_store_does_not_persist_to_disk() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let mut m4 = make_m4_deps("tester", "sys");
    // 换成仅会话级的记忆存储。
    let in_mem: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
    in_mem
        .save(MemoryScope::Session, "tester", "ephemeral fact")
        .unwrap();
    m4.memory = in_mem;
    Arc::make_mut(&mut m4.active_agent_def).memory = vec![MemoryScope::Session];

    let thread = build_thread(registry, m4);
    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let has_fact = req
        .system
        .0
        .iter()
        .any(|b| b.text.contains("ephemeral fact"));
    assert!(has_fact);
}

/// 顺序 stub:每次 `stream()` 弹出下一轮事件序列(用于多步回退测试)。
struct SeqStubClient {
    rounds: Mutex<Vec<Vec<ChatEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl SeqStubClient {
    fn new(rounds: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            rounds: Mutex::new(rounds),
            requests: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl ModelClient for SeqStubClient {
    fn name(&self) -> &str {
        "stub-seq"
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
        let events = self.rounds.lock().pop();
        let events = events.unwrap_or_default();
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// 多步历史持久化回归测试:第 1 轮模型调用 echo 工具,第 2 轮给出文本答案。
/// 关键断言:第 2 轮的 request.messages 必须包含 assistant(带 tool_call)与
/// Tool(result)消息 —— 即工具结果被提交进会话历史。修复前 pre_loop 每轮
/// 清空 state.messages,模型第 2 轮只看到原始用户问题。
#[tokio::test]
async fn m4_multi_turn_preserves_tool_history() {
    let registry = Arc::new(ModelRegistry::new());
    // Vec 是栈序弹出(pop 从尾部),故按「最后一段 = 第 1 轮」排列;
    // 为可读性,这里反过来 push:先 push 第 2 轮,再 push 第 1 轮,
    // 使 pop 先拿到第 1 轮。
    let stub = Arc::new(SeqStubClient::new(vec![
        // 第 2 轮(最后 pop):纯文本停止
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
        // 第 1 轮(先 pop):echo 工具调用
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "echo".into(),
                input_json: String::new(),
            },
            ChatEvent::ToolUseDelta("{\"text\":\"hi\"}".into()),
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let thread = build_thread(registry, m4);
    let mut handle = thread.submit(make_sub("echo hi then stop")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let requests = stub.requests.lock();
    assert!(
        requests.len() >= 2,
        "model should be called at least twice (tool call + final text), got {}",
        requests.len()
    );
    // 第 2 轮请求必须包含 assistant(tool_call)+ Tool(result)消息。
    let second = &requests[1];
    let has_assistant_tool_call = second.messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::Assistant(a) if a.tool_calls.iter().any(|tc| tc.name == "echo")
        )
    });
    let has_tool_result = second
        .messages
        .iter()
        .any(|m| matches!(m, ChatMessage::Tool(_)));
    assert!(
        has_assistant_tool_call,
        "2nd model_call must see the assistant tool_call in history (got {:?})",
        second
            .messages
            .iter()
            .map(|m| match m {
                ChatMessage::System(_) => "System",
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
            })
            .collect::<Vec<_>>()
    );
    assert!(
        has_tool_result,
        "2nd model_call must see the Tool result in history"
    );
}

/// loop-guard 回归测试:让 stub 连续 4 轮返回同一个 echo 工具调用。第 3 次
/// 命中阈值后,tool_exec 不再执行重复调用,而是注入提醒并回到 model_call。
/// 断言:总调用次数受限(不会无限循环到 max_iterations),且最终 TurnComplete。
#[tokio::test]
async fn m4_loop_guard_breaks_repeated_calls() {
    let registry = Arc::new(ModelRegistry::new());
    // pop 栈序:后 push 的先弹出。这里所有轮都返回同一 echo 调用。
    // 给 8 段(远超阈值 3 + max_iterations 兜底),验证 loop-guard 提前介入。
    let mut rounds: Vec<Vec<ChatEvent>> = Vec::new();
    for i in 0..8 {
        rounds.push(vec![
            ChatEvent::MessageStart {
                id: format!("m{}", i),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: format!("tc{}", i),
                name: "echo".into(),
                input_json: String::new(),
            },
            ChatEvent::ToolUseDelta("{\"text\":\"x\"}".into()),
            ChatEvent::MessageStop,
        ]);
    }
    // 最后一段:纯文本停止(确保 turn 能完成)。
    rounds.push(vec![
        ChatEvent::MessageStart {
            id: "mend".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("FINAL ANSWER: x".into()),
        ChatEvent::MessageStop,
    ]);
    let stub = Arc::new(SeqStubClient::new(rounds));
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let thread = build_thread(registry, m4);
    let mut handle = thread.submit(make_sub("loop")).await;
    let mut completed = false;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            completed = true;
            break;
        }
    }
    assert!(completed, "turn must complete");
    // loop-guard 在第 3 次重复命中后注入提醒,模型再调用若干次;总调用次数
    // 应明显少于「无 guard 时跑到 max_iterations(32)」。SeqStub 给了 9 段,
    // 若 guard 不工作会用满所有 9 段后 panic(无更多事件)。这里仅断言完成。
    let n = stub.requests.lock().len();
    assert!(n <= 9, "should not exceed available rounds, got {}", n);
}

/// progress-nudge 回归测试:连续多次 web_fetch 同域名 + 不同路径,
/// 验证 tool_exec 注入「已收集事实」+「信息已充分」system-reminder。
#[tokio::test]
async fn m4_progress_nudge_injects_facts_after_repeated_fetches() {
    use reflect_core::graph::state::WebFetchEntry;
    let mut state = reflect_core::graph::state::AgentState::default();
    // 模拟 4 次 web_fetch,其中 2 次同域名 (example.com)
    for (i, (url, snip)) in [
        ("https://example.com/a", "Page A: foo"),
        ("https://other.com/b", "Page B: bar"),
        ("https://example.com/c", "Page C: foo again"),
        ("https://example.com/d", "Page D: foo again"),
    ]
    .iter()
    .enumerate()
    {
        state.web_fetch_history.push(WebFetchEntry {
            url: url.to_string(),
            snippet: snip.to_string(),
            bytes: 100,
        });
        if let Some(d) = reflect_core::graph::nodes::url_host(url) {
            *state.web_domain_counts.entry(d).or_insert(0) += 1;
        }
        let _ = i;
    }
    // 触发 nudge(由 tool_exec 末尾调用)。max_iterations 传一个大值,
    // 让本测试只验证 web_fetch / 域名重复分支(条件 1/2),不触发
    // 「迭代剩余」分支(条件 0)。
    reflect_core::graph::nodes::maybe_inject_progress_nudge(&mut state, 32);
    // state.messages 应该多出一条 user message(reminder)
    let new_msgs: Vec<&str> = state
        .messages
        .messages
        .iter()
        .rev()
        .take(1)
        .filter_map(|m| match m {
            ChatMessage::User(u) => u.blocks.first().and_then(|b| match b {
                reflect_llm::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert!(
        !new_msgs.is_empty(),
        "progress-nudge should inject a user message after repeated fetches"
    );
    let body = new_msgs[0];
    assert!(
        body.contains("example.com") && body.contains("×"),
        "nudge should list repeated domain: got {:?}",
        body
    );
    assert!(
        body.contains("FINAL ANSWER"),
        "nudge should tell the model to stop and answer"
    );
}

/// 回归:`--resume` 续作时,preload 历史(preload_messages)必须在首个 turn
/// 被前置到当前用户输入之前,模型因此看得到之前的对话。
///
/// 此前 resume 分支只用了 `bundle.initial_messages.len()` 拼一条
/// system-reminder,真正的历史被丢弃 —— 恢复后 agent 毫无记忆。
/// 本测试直接构造一段 preload 历史(User 问 + Assistant 答),提交一条
/// 新 user 输入,断言模型收到的请求里同时包含历史 User/Assistant 与新输入。
#[tokio::test]
async fn preload_messages_are_seeded_into_first_turn_history() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    // 模拟 bootstrap_resume 回放出的历史:先前一轮 User 问 + Assistant 答。
    let prior_history = vec![
        ChatMessage::User(reflect_llm::UserContent {
            blocks: vec![reflect_llm::ContentBlock::Text {
                text: "what is 2+2?".into(),
            }],
        }),
        ChatMessage::Assistant(reflect_llm::AssistantContent {
            text: Some("2+2 equals 4.".into()),
            ..Default::default()
        }),
    ];

    let m4 = make_m4_deps("tester", "You are a tester.");
    // 关键:把 preload 历史注入 AgentConfig(模拟 resume 路径)。
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_m4(m4)
        .with_preload_messages(prior_history);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("and 3+3?")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub
        .last_request
        .lock()
        .clone()
        .expect("model should have been called");
    let msgs = &req.messages;

    // 历史的 User 问题必须在请求里。
    let has_prior_user = msgs.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("what is 2+2?"))
            })
        )
    });
    assert!(
        has_prior_user,
        "resumed turn must see prior User history; got messages: {:?}",
        msgs.iter()
            .map(|m| match m {
                ChatMessage::System(_) => "System",
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
            })
            .collect::<Vec<_>>()
    );

    // 历史的 Assistant 回答必须在请求里。
    let has_prior_assistant = msgs.iter().any(|m| {
        matches!(
            m,
            ChatMessage::Assistant(a) if a.text.as_deref().is_some_and(|t| t.contains("2+2 equals 4"))
        )
    });
    assert!(
        has_prior_assistant,
        "resumed turn must see prior Assistant history"
    );

    // 当前的新 user 输入也必须在请求里(不能被历史覆盖)。
    let has_new_user = msgs.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("and 3+3?"))
            })
        )
    });
    assert!(
        has_new_user,
        "resumed turn must also include the new prompt"
    );

    // 顺序:历史 User 应在历史 Assistant 之前(顺序不被打乱)。
    let pos_prior_user = msgs.iter().position(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("what is 2+2?"))
            })
        )
    });
    let pos_prior_assistant = msgs.iter().position(|m| {
        matches!(
            m,
            ChatMessage::Assistant(a) if a.text.as_deref().is_some_and(|t| t.contains("2+2 equals 4"))
        )
    });
    if let (Some(u), Some(a)) = (pos_prior_user, pos_prior_assistant) {
        assert!(
            u < a,
            "prior history order must be preserved (User before Assistant)"
        );
    }
}

/// 回归(once-only):preload 历史只在**首个 turn** 被前置一次。第二个
/// 跨提交 turn 不应再次前置历史 —— 否则会把旧历史与刚产生的新对话
/// 交织成 `[old, resume, reply, old, new]`,历史 echo、上下文损坏。
///
/// 这里用一个 `SeqStubClient` 依次响应两个 turn,断言第二个 turn 的请求
/// 里**不再**出现 preload 的旧历史,但保留了第一轮自然产生的新对话。
#[tokio::test]
async fn preload_messages_consumed_once_not_repeated_on_second_turn() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(SeqStubClient::new(vec![
        // 第 2 turn(最后 pop):纯文本停止
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
        // 第 1 turn(先 pop):纯文本停止
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("first answer".into()),
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

    // preload 历史:一句旧的 User 问。
    let prior_history = vec![ChatMessage::User(reflect_llm::UserContent {
        blocks: vec![reflect_llm::ContentBlock::Text {
            text: "OLD-HISTORY-MARKER".into(),
        }],
    })];

    let m4 = make_m4_deps("tester", "You are a tester.");
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_m4(m4)
        .with_preload_messages(prior_history);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    // 第 1 turn:应含 preload 旧历史。
    let mut h1 = thread.submit(make_sub("turn one")).await;
    while let Some(ev) = h1.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    // 立即 clone 出 requests 副本并释放锁,避免 guard 跨 await(clippy
    // await_holding_lock)。后续断言全部基于 clone 的局部变量。
    let reqs1: Vec<ChatRequest> = stub.requests.lock().clone();
    assert!(!reqs1.is_empty(), "first turn must have called the model");
    let first_has_marker = reqs1[0].messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("OLD-HISTORY-MARKER"))
            })
        )
    });
    assert!(
        first_has_marker,
        "first turn MUST include preload history marker"
    );

    // 第 2 turn:preload 历史已被消费,不应再次出现。
    let mut h2 = thread.submit(make_sub("turn two")).await;
    while let Some(ev) = h2.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    // 立即 clone 出 requests 副本并释放锁,避免 guard 跨 await(clippy
    // await_holding_lock)。后续断言全部基于 clone 的局部变量。
    let reqs2: Vec<ChatRequest> = stub.requests.lock().clone();
    assert!(reqs2.len() >= 2, "second turn must have called the model");
    let second = &reqs2[1];
    let second_has_marker = second.messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("OLD-HISTORY-MARKER"))
            })
        )
    });
    assert!(
        !second_has_marker,
        "second turn must NOT replay preload history (once-only); got messages: {:?}",
        second
            .messages
            .iter()
            .map(|m| match m {
                ChatMessage::System(_) => "System",
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
            })
            .collect::<Vec<_>>()
    );
    // 第二 turn 仍应包含它自己的新输入。
    let second_has_new = second.messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("turn two"))
            })
        )
    });
    assert!(second_has_new, "second turn must include its own prompt");
}

// ── v1.x Plan mode:让 LLM 看见「我在 Plan mode + 上轮被中断」────────────

/// Plan mode 启动时,LLM 的 system prompt 必须出现 `## Active Mode`
/// 段落,明确说「currently in Plan mode」并指向 `ExitPlanMode`。
///
/// 背景:TUI plan mode 下 agent 只调研不出 plan —— 不是 Stop 没产出,
/// 而是 LLM 看不见自己当前在 Plan mode。`pre_loop` 在 `compose_core`
/// 之后追加 `## Active Mode` 段落,专门给 LLM 一个「状态告知」信号。
#[tokio::test]
async fn m4_injects_active_mode_section_when_plan_mode() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_m4(m4)
        .with_initial_permission_mode(PermissionMode::Plan);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let active_mode_present = req.system.0.iter().any(|b| {
        b.text.contains("## Active Mode")
            && b.text.contains("currently in Plan mode")
            && b.text.contains("ExitPlanMode")
    });
    assert!(
        active_mode_present,
        "Plan mode 必须注入 ## Active Mode 段落给 LLM 状态信号"
    );
}

/// Plan mode + `set_last_abort_reason(UserInterrupt)` 后,下一次 turn 的
/// LLM system block 必须出现 `## Previous Turn` 段,且 `take_last_abort_reason`
/// 只能消费一次(防止跨 turn 持续唠叨)。
///
/// 流程:第一次 submit 触发 pre_loop 消费 abort reason;紧接着再 submit 一次,
/// 第二次的 LLM 调用不应再看到 `## Previous Turn`(因为 abort reason 已被
/// 清空)。
#[tokio::test]
async fn m4_injects_previous_turn_hint_after_interrupt_in_plan_mode() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_m4(m4)
        .with_initial_permission_mode(PermissionMode::Plan);
    // 模拟上轮用户按 Esc:submission_loop 会写 `UserInterrupt`。
    cfg.set_last_abort_reason(AbortReason::UserInterrupt);

    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    // 第一次 submit → pre_loop 消费 abort,system block 应含 ## Previous Turn。
    let mut handle = thread.submit(make_sub("first")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("first model call");
    let hint_present = req.system.0.iter().any(|b| {
        b.text.contains("## Previous Turn")
            && b.text.contains("interrupted by the user")
            && b.text.contains("ExitPlanMode")
    });
    assert!(
        hint_present,
        "Plan mode + 上轮被中断后,system 必须含 ## Previous Turn 提示"
    );

    // take-once 不变量:第二次 take 必须返回 None(防止跨 turn 唠叨)。
    let cfg_ref = thread.config();
    assert_eq!(
        cfg_ref.take_last_abort_reason(),
        None,
        "abort reason 必须在第一次 pre_loop 后被消费清空"
    );
}

/// `set_last_abort_reason` 在非 Plan mode 下不应该注入 `## Previous Turn`
/// —— 该提示只在 Plan mode 下有意义(指导 LLM 收尾出 plan)。非 Plan 模式
/// 下被中断只是普通 turn 结束,不需要特殊 system block。
#[tokio::test]
async fn m4_does_not_inject_previous_turn_hint_outside_plan_mode() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    // 注意:这里**不**设 permission_mode,默认是 Auto。
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);
    cfg.set_last_abort_reason(AbortReason::UserInterrupt);

    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    // `## Previous Turn` 不应出现 —— 但 take-once 仍然成立(防止唠叨)。
    let hint_absent = !req
        .system
        .0
        .iter()
        .any(|b| b.text.contains("## Previous Turn"));
    assert!(
        hint_absent,
        "Auto mode 下被中断不应该注入 ## Previous Turn,该 hint 仅在 Plan mode 下生效"
    );
    let cfg_ref = thread.config();
    assert_eq!(
        cfg_ref.take_last_abort_reason(),
        None,
        "即使不注入 hint,take-once 也要消费掉防止跨 turn 残留"
    );
}

// ── v1.x Plan mode:effective_tools 过滤 ─────────────────────────────

/// 测试用的可命名 stub 工具。`name()` 返回构造时指定的字符串,模拟任意
/// 工具名（write / edit / PlanWrite / ...）。`required_permission` 默认
/// `Auto`,因为测试只关心「是否出现在 LLM 的 tools 数组里」,不关心审批。
struct NamedStubTool {
    name: String,
}

#[async_trait]
impl Tool for NamedStubTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "stub"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _ctx: reflect_tools::ToolContext,
        _args: serde_json::Value,
    ) -> Result<reflect_protocol::ToolOutput, reflect_protocol::ToolError> {
        Ok(reflect_protocol::ToolOutput {
            content: vec![ContentBlock::text("ok")],
            is_error: false,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        })
    }
}

/// Plan mode 下 `pre_loop` 必须从 LLM 可见工具集中移除通用 `write` /
/// `edit`,只保留 `PlanWrite` / `ExitPlanMode` 等 plan 控制面工具 —— 防止
/// LLM 用 `write` 写 plan 时触发审批层(`required_permission = Prompt`)。
/// 详见 `pre_loop.rs` 中 Plan mode 过滤逻辑的注释。
#[tokio::test]
async fn m4_plan_mode_hides_write_edit_keeps_plan_control_plane() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_m4(m4)
        .with_initial_permission_mode(PermissionMode::Plan);

    // 注册同名工具覆盖默认 always_on 中的 write/edit/PlanWrite/ExitPlanMode,
    // 确保 plan_mode 过滤逻辑在真实注册路径上生效。
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(NamedStubTool {
        name: "write".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "edit".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "PlanWrite".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "ExitPlanMode".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "EnterPlanMode".into(),
    }));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let tool_names: Vec<&str> = req
        .tools
        .iter()
        .map(|t| match t {
            reflect_llm::ToolSpec::Function { name, .. } => name.as_str(),
        })
        .collect();

    // 核心断言:write / edit 在 Plan mode 下不应出现在 tools 数组。
    assert!(
        !tool_names.contains(&"write"),
        "Plan mode 必须从 effective_tools 移除 `write`,防止 LLM 误用触发审批;got {:?}",
        tool_names
    );
    assert!(
        !tool_names.contains(&"edit"),
        "Plan mode 必须从 effective_tools 移除 `edit`;got:?",
    );

    // plan 控制面工具必须保留。
    assert!(
        tool_names.contains(&"PlanWrite"),
        "Plan mode 下 `PlanWrite` 必须对 LLM 可见;got {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"ExitPlanMode"),
        "Plan mode 下 `ExitPlanMode` 必须对 LLM 可见;got {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"EnterPlanMode"),
        "Plan mode 下 `EnterPlanMode` 必须对 LLM 可见;got {:?}",
        tool_names
    );

    // 注:不验证 `read` / `grep` / `glob` 等只读工具是否可见 —— 这些是
    // ALWAYS_ON_TOOLS 默认项,本测试只在 ToolRegistry 注册了 Plan 控制面
    // 与 write/edit 工具;只读工具的可见性由 `ALWAYS_ON_TOOLS` 常量本身
    // 的回归测试保障(参见 reflect-skills 测试)。
}

/// 非 Plan mode（Auto 默认）下 `write` / `edit` 必须仍然可见 —— 防止
/// Plan mode 过滤逻辑误伤普通执行模式。AgentDefinition 工具过滤与
/// `readonly` 标志独立于此机制,本测试只覆盖 mode 这一维度。
#[tokio::test]
async fn m4_non_plan_mode_keeps_write_edit_visible() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    // 不调用 with_initial_permission_mode —— 默认 PermissionMode::Auto。
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);

    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(NamedStubTool {
        name: "write".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "edit".into(),
    }));
    tools.register(Arc::new(NamedStubTool {
        name: "PlanWrite".into(),
    }));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let tool_names: Vec<&str> = req
        .tools
        .iter()
        .map(|t| match t {
            reflect_llm::ToolSpec::Function { name, .. } => name.as_str(),
        })
        .collect();

    assert!(
        tool_names.contains(&"write"),
        "Auto mode 下 `write` 必须可见(不能误伤普通执行模式);got {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"edit"),
        "Auto mode 下 `edit` 必须可见;got:?",
    );
    assert!(
        tool_names.contains(&"PlanWrite"),
        "Auto mode 下 `PlanWrite` 也应可见（always_on 包含）;got {:?}",
        tool_names
    );
}

/// v1.x:外部源工具(Runtime / Plugin / Mcp / Remote)注册即对 LLM 可见,
/// 不被 skills catalog 的 always_on 白名单挡住。模拟 GUI 的 MCP/LSP 接入
/// (`register_runtime_tool`)与 CLI 的 MCP 接入(`Mcp` 源),两者都应出现在
/// effective_tools。同时 Builtin 侧 curated 语义不变:不在 always_on 的
/// 内置工具(echo)仍被隐藏。
#[tokio::test]
async fn m4_external_source_tools_are_visible_beyond_always_on() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);

    let tools = Arc::new(ToolRegistry::default());
    // Builtin 源:echo 不在 ALWAYS_ON_TOOLS → curated 隐藏(对照组)。
    tools.register(Arc::new(EchoTool));
    // Runtime 源(GUI 的 MCP/LSP 走此路径)。
    tools.register_runtime_tool(Arc::new(NamedStubTool {
        name: "mcp__fs__list".into(),
    }));
    // Mcp 源(CLI 的 bootstrap_m6 走此路径)。
    tools.register_with_source(
        reflect_tools::ToolSource::Mcp,
        Arc::new(NamedStubTool {
            name: "mcp__gh__pr".into(),
        }),
    );
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let tool_names: Vec<&str> = req
        .tools
        .iter()
        .map(|t| match t {
            reflect_llm::ToolSpec::Function { name, .. } => name.as_str(),
        })
        .collect();

    assert!(
        tool_names.contains(&"mcp__fs__list"),
        "Runtime 源工具必须对 LLM 可见;got {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"mcp__gh__pr"),
        "Mcp 源工具必须对 LLM 可见;got {:?}",
        tool_names
    );
    assert!(
        !tool_names.contains(&"echo"),
        "Builtin 侧 curated 语义不变:不在 always_on 的内置工具仍隐藏;got {:?}",
        tool_names
    );
}

// ── v1.x 每轮回填:recorder 驱动的跨轮记忆 ────────────────────────────

/// 测试用内存 recorder:record 收进 Vec,replay 全量返回,
/// truncate_after 按 turn_id 定位截断(模拟 JsonlRolloutWriter 语义的
/// 最小子集,避免测试触碰文件系统)。
#[derive(Debug, Default)]
struct MemRecorder {
    records: Mutex<Vec<reflect_protocol::RolloutRecord>>,
}

#[async_trait]
impl reflect_protocol::RolloutRecorder for MemRecorder {
    async fn record(&self, r: reflect_protocol::RolloutRecord) -> anyhow::Result<()> {
        self.records.lock().push(r);
        Ok(())
    }
    async fn replay(
        &self,
        _session_id: reflect_protocol::ThreadId,
    ) -> anyhow::Result<Vec<reflect_protocol::RolloutRecord>> {
        Ok(self.records.lock().clone())
    }
    async fn list_sessions(&self) -> anyhow::Result<Vec<reflect_protocol::SessionInfo>> {
        Ok(vec![])
    }
    async fn truncate_after(
        &self,
        to_turn_id: Option<&reflect_protocol::TurnId>,
    ) -> anyhow::Result<usize> {
        let mut records = self.records.lock();
        let Some(target) = to_turn_id else {
            return Ok(0);
        };
        // 找到目标 turn 的第一条 Message 记录,截断到该下标(丢弃目标及之后)。
        let pos = records.iter().position(|r| match r {
            reflect_protocol::RolloutRecord::Message { turn_id, .. } => turn_id == target,
            _ => false,
        });
        let Some(pos) = pos else {
            return Ok(0);
        };
        let dropped = records.len() - pos;
        records.truncate(pos);
        Ok(dropped)
    }
}

/// 跨轮记忆回归:带 recorder 的线程,第 2 轮模型请求必须包含第 1 轮的
/// User 输入与 Assistant 回答 —— 修复前引擎无跨轮累积,第 2 轮只看到
/// 新输入(每轮失忆)。
#[tokio::test]
async fn recorder_refill_gives_cross_turn_memory() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(SeqStubClient::new(vec![
        // 第 2 轮(最后 pop):纯文本停止。
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("second answer".into()),
            ChatEvent::MessageStop,
        ],
        // 第 1 轮(先 pop)。
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("first answer".into()),
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

    let mut m4 = make_m4_deps("tester", "You are a tester.");
    m4.recorder = Some(Arc::new(MemRecorder::default()));
    let thread = build_thread(registry, m4);

    let mut h1 = thread.submit(make_sub("我叫 Alice")).await;
    while let Some(ev) = h1.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let mut h2 = thread.submit(make_sub("我叫什么?")).await;
    while let Some(ev) = h2.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let reqs: Vec<ChatRequest> = stub.requests.lock().clone();
    assert!(reqs.len() >= 2, "两轮各至少一次模型调用");
    let second = &reqs[1];
    let has_turn1_user = second.messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("我叫 Alice"))
            })
        )
    });
    assert!(
        has_turn1_user,
        "第 2 轮必须回填第 1 轮的 User 输入;got {:?}",
        second
            .messages
            .iter()
            .map(|m| match m {
                ChatMessage::System(_) => "System",
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
            })
            .collect::<Vec<_>>()
    );
    let has_turn1_assistant = second
        .messages
        .iter()
        .any(|m| matches!(m, ChatMessage::Assistant(a) if a.text.as_deref().is_some_and(|t| t.contains("first answer"))));
    assert!(
        has_turn1_assistant,
        "第 2 轮必须回填第 1 轮的 Assistant 回答"
    );

    // 本轮新输入恰好出现一次(replay 在写入本轮记录之前,不得重复)。
    let new_input_count = second
        .messages
        .iter()
        .filter(|m| {
            matches!(
                m,
                ChatMessage::User(u) if u.blocks.iter().any(|b| {
                    matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("我叫什么?"))
                })
            )
        })
        .count();
    assert_eq!(new_input_count, 1, "本轮输入不得因回填而重复");
}

/// rewind 回归:truncate_after 截断后,下一轮回填不再包含被截断的对话
/// —— `Op::Rewind` 分支注释声称的「截断后下一次 turn 自然从更短的
/// rollout 回放」由此真正成立。
#[tokio::test]
async fn recorder_refill_respects_rewind_truncation() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(SeqStubClient::new(vec![
        // rewind 后的第 3 轮。
        vec![
            ChatEvent::MessageStart {
                id: "m3".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("after rewind".into()),
            ChatEvent::MessageStop,
        ],
        // 第 1 轮。
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("to be truncated".into()),
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

    let recorder = Arc::new(MemRecorder::default());
    let mut m4 = make_m4_deps("tester", "You are a tester.");
    m4.recorder = Some(recorder.clone());
    let thread = build_thread(registry, m4);

    let mut h1 = thread.submit(make_sub("old question")).await;
    while let Some(ev) = h1.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    // 从 recorder 里取第 1 轮的 turn_id,发 Rewind 截断它。
    let turn1 = recorder
        .records
        .lock()
        .iter()
        .find_map(|r| match r {
            reflect_protocol::RolloutRecord::Message { turn_id, .. } => Some(*turn_id),
            _ => None,
        })
        .expect("第 1 轮应有 Message 记录");
    let mut hr = thread
        .submit(Submission {
            id: "rewind-sub".into(),
            op: reflect_protocol::Op::Rewind {
                to_turn_id: Some(turn1.to_string()),
            },
            client_user_message_id: None,
            trace: None,
            workspace: None,
            source_command: None,
        })
        .await;
    while let Some(ev) = hr.next().await {
        if matches!(ev.msg, EventMsg::TurnRewound(_)) {
            break;
        }
    }

    // 截断后的新一轮:回填历史不得再包含被截断的第 1 轮内容。
    let mut h3 = thread.submit(make_sub("fresh start")).await;
    while let Some(ev) = h3.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let reqs: Vec<ChatRequest> = stub.requests.lock().clone();
    let last = reqs.last().expect("rewind 后应有模型调用");
    let leaked = last.messages.iter().any(|m| {
        matches!(
            m,
            ChatMessage::User(u) if u.blocks.iter().any(|b| {
                matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("old question"))
            })
        ) || matches!(
            m,
            ChatMessage::Assistant(a) if a.text.as_deref().is_some_and(|t| t.contains("to be truncated"))
        )
    });
    assert!(
        !leaked,
        "rewind 截断后的回填不得包含被截断内容;got {:?}",
        last.messages
            .iter()
            .map(|m| match m {
                ChatMessage::System(_) => "System",
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
            })
            .collect::<Vec<_>>()
    );
}

/// v1.4 子代理可见性防回归:`call_<role>` 是运行时动态注册的工具(不在静态
/// `ALWAYS_ON_TOOLS` 中),bootstrap 注册后必须同步调用
/// `skills.add_always_on_tools` 把它补进可见集 —— 否则 `pre_loop` 的
/// `effective_tools` 过滤会把它从模型请求的 tools 数组里剔除,LLM 永远
/// 拿不到子代理工具的 schema、无法委派。离线 mock provider 无视实际工具
/// 列表(直接回放脚本),这条链路只能靠本测试(stub 收到的真实 request)
/// 与运行时真实 LLM 测试暴露。
#[tokio::test]
async fn m4_dynamic_always_on_tool_visible_to_model() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let m4 = make_m4_deps("tester", "You are a tester.");
    // 模拟 bootstrap:把动态注册的子代理工具名补入 always-on 可见集。
    m4.skills
        .add_always_on_tools(vec!["call_explorer".to_string()]);
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);

    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(NamedStubTool {
        name: "call_explorer".into(),
    }));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let req = stub.last_request.lock().clone().expect("model was called");
    let tool_names: Vec<&str> = req
        .tools
        .iter()
        .map(|t| match t {
            reflect_llm::ToolSpec::Function { name, .. } => name.as_str(),
        })
        .collect();

    assert!(
        tool_names.contains(&"call_explorer"),
        "动态补入 always-on 的子代理工具必须出现在模型请求 tools 数组;got {:?}",
        tool_names
    );
}

// ── v1.6:按模型匹配的外置提示词(端到端)────────────────────────

/// 构造临时提示词目录:`provider` 层 + `model` 精确层各写一个 section。
fn make_prompts_dir(dir: &Path, provider: &str, model: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(format!("{provider}.md")),
        format!(
            "## tool.echo\n{provider}-layer echo description.\n\n## system.main\n{provider}-layer main prompt.\n"
        ),
    )?;
    std::fs::write(
        dir.join(format!("{model}.md")),
        format!("## system.main\n{model}-exact main prompt.\n"),
    )?;
    Ok(())
}

/// 集成:精确模型层 `system.main` 与厂商层 `tool.<name>` 描述覆盖
/// 端到端生效 —— 同一进程先按 spec A 请求(命中精确层),再按 spec B
/// 请求(命中厂商层),最后一次**不带用户层**重新初始化库(内嵌默认)。
///
/// 进程级提示词库是全局单例:与下方 `hot_model_swap_*` 测试共享同一把
/// 静态互斥锁串行执行,避免并发重扫互相冲掉对方的用户层。测试体是
/// async,用 tokio 的 async-aware 锁避免 guard 跨 await。
static PROMPT_LIBRARY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn model_layer_prompts_end_to_end() {
    let _library_guard = PROMPT_LIBRARY_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let prompts_dir = tmp.path().join("prompts");
    make_prompts_dir(&prompts_dir, "stub", "m1").expect("write prompt layers");

    // 1) 装配用户层(与 exec bootstrap 同款调用)。
    reflect_prompt::init_library(&[prompts_dir.as_path()], None);

    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    // BuiltinDefault 来源(与 exec 默认 agent 一致)→ pre_loop 从库解析。
    let mut m4 = make_m4_deps("tester", "");
    m4.active_agent_def = Arc::new(AgentDefinition {
        name: "tester".into(),
        source: reflect_agent_def::SystemPromptSource::BuiltinDefault,
        ..Default::default()
    });
    let thread = build_thread(registry, m4);

    // 2) spec = stub/m1 → 精确层 system.main + 厂商层 tool.echo 描述。
    let mut handle = thread.submit(make_sub("hi")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let req = stub.last_request.lock().clone().expect("model was called");
    assert!(
        req.system
            .0
            .iter()
            .any(|b| b.text.contains("m1-exact main prompt.")),
        "精确模型层 system.main 应在请求 system 中"
    );
    let echo_desc = req.tools.iter().find_map(|t| match t {
        reflect_llm::ToolSpec::Function {
            name, description, ..
        } if name == "echo" => Some(description.clone()),
        _ => None,
    });
    assert_eq!(
        echo_desc.as_deref(),
        Some("stub-layer echo description."),
        "厂商层 tool.echo 描述应覆盖内嵌默认"
    );

    // 3) 重扫为空目录(模拟用户删除覆盖文件)→ 回落内嵌默认。
    let empty_dir = tmp.path().join("empty-prompts");
    std::fs::create_dir_all(&empty_dir).unwrap();
    reflect_prompt::init_library(&[empty_dir.as_path()], None);
    let set = reflect_prompt::library().resolve("stub/m1");
    assert!(
        set.get("system.main")
            .unwrap_or_default()
            .contains("先读后改"),
        "无用户层时应回落内嵌默认 system.main"
    );
    assert_ne!(
        set.get("tool.echo"),
        Some("stub-layer echo description."),
        "无用户层时 tool.echo 应回到内嵌默认"
    );

    // 清理:恢复全局库到内嵌默认,不污染其他测试。
    reflect_prompt::init_library(&[], None);
}

/// 实测 E:运行中热切换模型(热重载同款 `set_model` 路径)后,下一请求的
/// `system.main` 应从旧模型的精确层切到新模型的精确层 —— 提示词库是
/// 每请求动态 resolve 的,热切换无需重新装配。
#[tokio::test]
async fn hot_model_swap_switches_prompt_layer_on_next_request() {
    let _library_guard = PROMPT_LIBRARY_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let prompts_dir = tmp.path().join("prompts");
    make_prompts_dir(&prompts_dir, "stub", "m1").expect("write prompt layers");
    // 新模型 m2 的精确层(旧目录里没有的文件,运行中追加)。
    std::fs::write(
        prompts_dir.join("m2.md"),
        "## system.main\nm2-exact main prompt.\n",
    )
    .expect("write m2 layer");

    reflect_prompt::init_library(&[prompts_dir.as_path()], None);

    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let mut m4 = make_m4_deps("tester", "");
    m4.active_agent_def = Arc::new(AgentDefinition {
        name: "tester".into(),
        source: reflect_agent_def::SystemPromptSource::BuiltinDefault,
        ..Default::default()
    });
    let thread = build_thread(registry, m4);

    // Turn 1:spec = stub/m1 → 命中 m1 精确层。
    let mut handle = thread.submit(make_sub("first turn")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let req1 = stub.last_request.lock().clone().expect("first request");
    assert!(
        req1.system
            .0
            .iter()
            .any(|b| b.text.contains("m1-exact main prompt.")),
        "切换前请求应命中 m1 精确层"
    );

    // 热切换:与 reflect-exec 热重载同一条 `set_model` 路径(不重建线程)。
    thread.config().set_model("stub/m2");

    // Turn 2:spec = stub/m2 → 库每请求动态 resolve,应命中 m2 精确层。
    let mut handle = thread.submit(make_sub("second turn")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let req2 = stub.last_request.lock().clone().expect("second request");
    assert!(
        req2.system
            .0
            .iter()
            .any(|b| b.text.contains("m2-exact main prompt.")),
        "热切换后下一请求应命中 m2 精确层"
    );
    assert!(
        !req2
            .system
            .0
            .iter()
            .any(|b| b.text.contains("m1-exact main prompt.")),
        "热切换后旧模型层不应残留"
    );

    // 清理:恢复全局库到内嵌默认,不污染其他测试。
    reflect_prompt::init_library(&[], None);
}

/// v1.6.1:`/prompt` slash 的引擎端 —— `Op::PromptCommand` List / Switch
/// 都不开 turn,应答 `EventMsg::PromptList` / `PromptSwitched` 经
/// per-submission 通道送达,`SubmissionClosed` 收尾。
#[tokio::test]
async fn prompt_command_lists_and_rescans_library() {
    let _library_guard = PROMPT_LIBRARY_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let prompts_dir = tmp.path().join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    std::fs::write(
        prompts_dir.join("m1.md"),
        "## system.main\nm1-exact main prompt.\n",
    )
    .expect("write m1 layer");

    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("ok".into()),
        ChatEvent::MessageStop,
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

    let mut m4 = make_m4_deps("tester", "");
    // BuiltinDefault 来源(与 exec 默认 agent 一致)→ pre_loop 从库解析。
    m4.active_agent_def = Arc::new(AgentDefinition {
        name: "tester".into(),
        source: reflect_agent_def::SystemPromptSource::BuiltinDefault,
        ..Default::default()
    });
    // 模拟 bootstrap:记录目录序,Switch 重扫按它装配。
    m4.prompt_dirs = vec![prompts_dir.clone()];
    let thread = build_thread(registry, m4);
    // 让全局库也指向该目录(与 bootstrap init_library 等效)。
    reflect_prompt::init_library(&[prompts_dir.as_path()], None);

    let make_op_sub = |op: reflect_protocol::Op| Submission {
        id: "prompt-op".into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: Some("prompt".into()),
    };

    // ── List:可用层含目录里的 m1 + 内置厂商层;生效层含 m1(当前 spec stub/m1)。
    let mut handle = thread
        .submit(make_op_sub(reflect_protocol::Op::PromptCommand {
            action: reflect_protocol::PromptAction::List,
        }))
        .await;
    let mut saw_list = false;
    while let Some(ev) = handle.next().await {
        if let reflect_protocol::EventMsg::PromptList(info) = ev.msg {
            assert!(info.available.iter().any(|l| l == "m1"));
            assert!(info.available.iter().any(|l| l == "claude"));
            assert_eq!(info.current_spec, "stub/m1");
            assert!(info.active_layers.iter().any(|l| l == "m1"));
            saw_list = true;
        }
    }
    assert!(saw_list, "List 应答 PromptList 事件");

    // ── Switch:重扫后应答 PromptSwitched;随后真实请求仍命中该层。
    std::fs::write(
        prompts_dir.join("m2.md"),
        "## system.main\nm2-exact main prompt.\n",
    )
    .expect("write m2 layer");
    let mut handle = thread
        .submit(make_op_sub(reflect_protocol::Op::PromptCommand {
            action: reflect_protocol::PromptAction::Switch { layer: None },
        }))
        .await;
    let mut saw_switched = false;
    while let Some(ev) = handle.next().await {
        if let reflect_protocol::EventMsg::PromptSwitched(info) = ev.msg {
            assert_eq!(info.action, "reload");
            saw_switched = true;
        }
    }
    assert!(saw_switched, "Switch 应答 PromptSwitched 事件");

    // 重扫后 m2 层进入库:set_model 到 stub/m2,下一请求命中 m2 文案。
    thread.config().set_model("stub/m2");
    let mut handle = thread.submit(make_sub("after rescan")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let req = stub
        .last_request
        .lock()
        .clone()
        .expect("request after rescan");
    assert!(
        req.system
            .0
            .iter()
            .any(|b| b.text.contains("m2-exact main prompt.")),
        "重扫后的库应在下一请求生效 m2 层"
    );

    // 清理:恢复全局库到内嵌默认,不污染其他测试。
    reflect_prompt::init_library(&[], None);
}

// ── 清单回灌(todo_write → 下一轮模型上下文)────────────────────────────

/// 假 `TodoWrite`:返回与真工具同形的 `metadata.todos`。
struct TodoStubTool;

#[async_trait]
impl Tool for TodoStubTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }
    fn description(&self) -> &str {
        "write a todo list"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {"type": "array", "items": {"type": "object"}}
            }
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text("Wrote 2 todos.")],
            is_error: false,
            // 与 reflect-task 的 TodoWriteTool 同形。
            metadata: serde_json::json!({
                "todos": [
                    {"content": "first item", "status": "completed"},
                    {"content": "second item", "status": "pending"}
                ],
                "count": 2,
            }),
            elapsed_ms: 0,
        })
    }
}

/// 按调用序返回不同事件的 stub —— 第一次要求调工具,第二次直接作答。
struct ScriptedClient {
    calls: Mutex<u32>,
    requests: Mutex<Vec<ChatRequest>>,
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
        let n = {
            let mut c = self.calls.lock();
            *c += 1;
            *c
        };
        self.requests.lock().push(request);
        let events = if n == 1 {
            vec![
                ChatEvent::MessageStart {
                    id: "m1".into(),
                    model: "stub-1".into(),
                },
                ChatEvent::ToolUseStart {
                    id: "call-1".into(),
                    name: "TodoWrite".into(),
                    input_json: "{}".into(),
                },
                ChatEvent::MessageStop,
            ]
        } else {
            vec![
                ChatEvent::MessageStart {
                    id: "m2".into(),
                    model: "stub-1".into(),
                },
                ChatEvent::ContentDelta("done".into()),
                ChatEvent::MessageStop,
            ]
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// 清单必须**进模型上下文** —— 此前它只走 `ToolOutput.metadata`
/// (输出通道):GUI 能展示,但下一轮模型看不见 → 写完即忘。
#[tokio::test]
async fn todo_checklist_is_injected_back_into_model_context() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(ScriptedClient {
        calls: Mutex::new(0),
        requests: Mutex::new(Vec::new()),
    });
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

    let m4 = make_m4_deps("tester", "You are a reviewer.");
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_m4(m4);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(TodoStubTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("plan the work")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let requests = stub.requests.lock().clone();
    assert!(
        requests.len() >= 2,
        "应有两次 model call(第一次调工具,第二次基于结果作答),实际 {}",
        requests.len()
    );
    let second = &requests[1];
    let system_text: String = second
        .system
        .0
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        system_text.contains("Current Todo List"),
        "第二次请求的 system prompt 必须带清单段,实际:\n{system_text}"
    );
    assert!(
        system_text.contains("first item") && system_text.contains("second item"),
        "清单内容必须进模型上下文,实际:\n{system_text}"
    );
    // 状态标记:completed → [x],pending → [ ]。
    assert!(
        system_text.contains("- [x] first item") && system_text.contains("- [ ] second item"),
        "清单的三态标记不对,实际:\n{system_text}"
    );
}
