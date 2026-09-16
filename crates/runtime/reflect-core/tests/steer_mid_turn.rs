//! v1.4 A2 — `Op::Steer` 回合中途转向注入端到端。
//!
//! 流程:模型第一次调用发起慢工具调用 → 工具执行期间客户端提交
//! `Op::Steer` → ToolExec → PreLoop 回环入口收割转向消息注入历史 →
//! 模型第二次调用(带新增的用户消息)收口。断言第二次请求的 messages
//! 含转向文本:`Now` 优先级为纯文本(用户中途说话),`Attachment`
//! 优先级被 `<system-reminder>` 包裹(参考资料,不冒充指令)。

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
use reflect_core::{AgentConfig, AgentThread};
use reflect_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_memory::{FileMemoryStore, MemoryScope, MemoryStore};
use reflect_prompt::PromptBuilder;
use reflect_protocol::{
    ContentBlock as _Pb, EventMsg, Op, SteeringPriorityMirror, Submission, UserInputItem,
};
use reflect_skills::SkillsCatalog;
use reflect_tools::{Tool, ToolContext, ToolOutput, ToolRegistry, ToolSource, builtins::EchoTool};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// 按调用序号回放脚本的 stub 模型:第 1 次发起慢工具调用,第 2 次纯文本
/// 收口;同时记录每次请求的 messages 供断言。
struct ScriptedClient {
    call_count: std::sync::atomic::AtomicUsize,
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
        use std::sync::atomic::Ordering;
        self.requests.lock().push(request);
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            // 第一次:发起 slow_tool 调用,让 turn 进入 ToolExec 阶段。
            vec![
                ChatEvent::MessageStart {
                    id: "m1".into(),
                    model: "scripted".into(),
                },
                ChatEvent::ToolUseStart {
                    id: "c1".into(),
                    name: "slow_tool".into(),
                    input_json: "{}".into(),
                },
                ChatEvent::MessageStop,
            ]
        } else {
            // 后续:纯文本收口(注入的 steering 已在上下文里)。
            vec![
                ChatEvent::MessageStart {
                    id: format!("m{}", n + 1),
                    model: "scripted".into(),
                },
                ChatEvent::ContentDelta("done".into()),
                ChatEvent::MessageStop,
            ]
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// 慢工具:执行耗时 300ms,给测试线程留出发 Op::Steer 的窗口。
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "sleep 300ms then echo"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, reflect_tools::ToolError> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text("slept")],
            is_error: false,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        })
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

fn make_m4() -> M4Deps {
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(NoopSummarizer),
    ));
    let tmp = std::env::temp_dir().join(format!("reflect-steer-{}", uuid::Uuid::new_v4()));
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
    #[allow(clippy::field_reassign_with_default)]
    {
        def.name = "tester".to_string();
        def.description = "test".into();
        def.system_prompt = "You are a tester.".into();
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
    }
}

fn sub(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
    }
}

/// 驱动一个「工具调用 → 转向注入 → 收口」回合,返回两次模型请求。
async fn run_turn_with_steering(
    priority: SteeringPriorityMirror,
    steer_text: &str,
) -> Vec<ChatRequest> {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(ScriptedClient {
        call_count: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    registry.register_pool(
        "scripted",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    let cfg = AgentConfig::new("scripted/m1", Path::new(".")).with_m4(make_m4());
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    // 插件源注册:外部工具不受 skills catalog 的 always_on 白名单限制,
    // pre_loop 会把它们并入 LLM 可见集(effective.extend(external))。
    tools.register_with_source(ToolSource::Plugin, Arc::new(SlowTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread
        .submit(sub(
            "t1",
            Op::UserInput {
                items: vec![UserInputItem::Text {
                    text: "跑一个慢工具".into(),
                }],
                thread_settings: Default::default(),
            },
        ))
        .await;

    // 等到工具开始执行(ToolCallBegin)再投喂转向 —— 确保注入发生在
    // ToolExec → PreLoop 回环(而非 turn 边界合并路径)。
    let mut steering_sent = false;
    let deadline = Duration::from_secs(10);
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event in time")
    {
        if matches!(ev.msg, EventMsg::ToolCallBegin(_)) && !steering_sent {
            steering_sent = true;
            let _steer_handle = thread
                .submit(sub(
                    "t1-steer",
                    Op::Steer {
                        priority,
                        items: vec![UserInputItem::Text {
                            text: steer_text.into(),
                        }],
                    },
                ))
                .await;
        }
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    assert!(steering_sent, "必须在工具执行期间发出 Op::Steer");

    let reqs = stub.requests.lock().clone();
    assert!(reqs.len() >= 2, "模型应被调用至少两次,实际 {}", reqs.len());
    reqs
}

/// 收集请求中全部用户消息的文本。
fn user_texts(req: &ChatRequest) -> Vec<String> {
    req.messages
        .iter()
        .filter_map(|m| match m {
            ChatMessage::User(uc) => Some(
                uc.blocks
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
        .collect()
}

/// Now 优先级:第二次模型请求含转向文本的纯 User 消息(无标签包裹)。
#[tokio::test]
async fn steer_now_injected_mid_turn_as_plain_user_message() {
    let reqs = run_turn_with_steering(SteeringPriorityMirror::Now, "改用中文回答").await;
    let second = &reqs[1];
    let texts = user_texts(second);
    let hit = texts
        .iter()
        .find(|t| t.contains("改用中文回答"))
        .expect("第二次请求应含转向文本");
    assert!(
        !hit.contains("<system-reminder>"),
        "Now 优先级必须是纯文本,不应包 system-reminder:{hit}"
    );
}

/// Attachment 优先级:注入文本被 `<system-reminder>` 包裹。
#[tokio::test]
async fn steer_attachment_wrapped_in_system_reminder() {
    let reqs =
        run_turn_with_steering(SteeringPriorityMirror::Attachment, "参考:配置在 foo.toml").await;
    let second = &reqs[1];
    let texts = user_texts(second);
    assert!(
        texts.iter().any(|t| {
            t.contains("<system-reminder>") && t.contains("参考:配置在 foo.toml")
        }),
        "Attachment 应以 system-reminder 包裹注入,实际用户消息:{texts:?}"
    );
}

/// 第一次请求不应含转向文本(注入发生在回环之后)。
#[tokio::test]
async fn steer_not_present_in_first_call() {
    let reqs = run_turn_with_steering(SteeringPriorityMirror::Now, "改用中文回答").await;
    assert!(
        !user_texts(&reqs[0])
            .iter()
            .any(|t| t.contains("改用中文回答")),
        "第一次调用不应看到后到的转向消息"
    );
}

// 引用 protocol ContentBlock 以保持导入完整(serde 断言备用)。
#[allow(dead_code)]
type _Keep = _Pb;
