//! v1.5 E1 — 新增 hook 触点端到端:UserPromptSubmit / PreCompact。
//!
//! 用 ShellHook 作数据驱动的裁决源(Claude Code 式外部命令),验证:
//! - UserPromptSubmit deny → prompt 不进模型、不落盘,Error + TurnAborted;
//! - UserPromptSubmit inject → 引导文本以 system-reminder 进入首请求;
//! - PreCompact deny → 压缩被跳过(ContextCompacted 不发)。

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
use reflect_hooks::{HookEngine, HookEventKind, ShellHook};
use reflect_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use reflect_memory::{FileMemoryStore, MemoryStore};
use reflect_prompt::PromptBuilder;
use reflect_protocol::{EventMsg, Op, Submission, UserInputItem};
use reflect_skills::SkillsCatalog;
use reflect_tools::ToolRegistry;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct ScriptedClient {
    scripts: std::sync::Mutex<Vec<Vec<ChatEvent>>>,
    requests: std::sync::Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ModelClient for ScriptedClient {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().unwrap().push(request);
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

fn make_m4(compactor: Arc<Compactor>) -> M4Deps {
    let tmp = std::env::temp_dir().join(format!("reflect-hook-points-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let memory: Arc<dyn MemoryStore> = Arc::new(FileMemoryStore::new(&tmp, &tmp));
    M4Deps {
        compactor,
        memory,
        skills: Arc::new(SkillsCatalog::new()),
        prompt_builder: Arc::new(Mutex::new(PromptBuilder::new())),
        active_agent_def: Arc::new(AgentDefinition {
            name: "hook-points".into(),
            description: "test".into(),
            system_prompt: "You are a hook-point test agent.".into(),
            ..Default::default()
        }),
        recorder: None,
        note_store: Arc::new(reflect_notes::InMemoryNoteStore::new()),
        file_recovery: Arc::new(reflect_recovery::ActiveFileRecovery::new(Arc::from(tmp))),
        subagent_registry: reflect_recovery::SubagentRegistry::shared(),
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

fn user_input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInputItem::Text { text: text.into() }],
        thread_settings: Default::default(),
    }
}

fn build(client: Arc<ScriptedClient>, m4: M4Deps, engine: Option<Arc<HookEngine>>) -> AgentThread {
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
    let cfg = AgentConfig::new("scripted/m1", Path::new(".")).with_m4(m4);
    // 引擎经 AgentThread::new 第 5 参注入(与 exec bootstrap 同路径)。
    AgentThread::new(
        cfg,
        registry,
        tools,
        None,
        Some(engine.unwrap_or_else(|| Arc::new(HookEngine::new()))),
    )
}

/// UserPromptSubmit deny:prompt 不进模型(requests 为空)、回合以
/// Error + TurnAborted(prompt_rejected)收场。
#[tokio::test]
async fn user_prompt_submit_deny_rejects_turn() {
    let engine = Arc::new(HookEngine::new());
    engine.register(ShellHook::new(
        "gate",
        HookEventKind::UserPromptSubmit,
        None,
        "echo '{\"decision\":\"deny\",\"reason\":\"off-topic\"}'",
        std::time::Duration::from_secs(5),
    ));
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![vec![
            ChatEvent::MessageStart {
                id: "m".into(),
                model: "scripted".into(),
            },
            ChatEvent::MessageStop,
        ]]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let m4 = make_m4(Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    )));
    let thread = build(client.clone(), m4, Some(engine));

    let mut h = thread.submit(sub("u1", user_input("请帮我干私活"))).await;
    let deadline = Duration::from_secs(10);
    let mut saw_error = false;
    let mut saw_abort = false;
    while let Some(ev) = timeout(deadline, h.next()).await.expect("event in time") {
        match ev.msg {
            EventMsg::Error(e) => {
                assert!(e.message.contains("off-topic"), "拒绝原因应透传: {e:?}");
                saw_error = true;
            }
            EventMsg::TurnAborted(a) => match a.reason {
                reflect_protocol::AbortReason::Error { code, .. } => {
                    assert_eq!(code, "prompt_rejected");
                    saw_abort = true;
                }
                other => panic!("wrong abort reason: {other:?}"),
            },
            EventMsg::TurnComplete(_) => panic!("被拒回合不应有 TurnComplete"),
            _ => {}
        }
    }
    assert!(saw_error && saw_abort);
    assert!(
        client.requests.lock().unwrap().is_empty(),
        "被拒 prompt 不应进模型"
    );
}

/// UserPromptSubmit inject:引导文本以 system-reminder 进入首请求。
#[tokio::test]
async fn user_prompt_submit_inject_adds_guidance() {
    let engine = Arc::new(HookEngine::new());
    engine.register(ShellHook::new(
        "guide",
        HookEventKind::UserPromptSubmit,
        None,
        "echo '{\"decision\":\"inject\",\"message\":\"Always answer in bullet points.\"}'",
        std::time::Duration::from_secs(5),
    ));
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![vec![
            ChatEvent::MessageStart {
                id: "m".into(),
                model: "scripted".into(),
            },
            ChatEvent::MessageStop,
        ]]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let m4 = make_m4(Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    )));
    let thread = build(client.clone(), m4, Some(engine));

    let mut h = thread.submit(sub("u2", user_input("总结一下"))).await;
    let deadline = Duration::from_secs(10);
    while let Some(ev) = timeout(deadline, h.next()).await.expect("event in time") {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let reqs = client.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    let has_guidance = reqs[0].messages.iter().any(|m| match m {
        ChatMessage::User(u) => u.blocks.iter().any(|b| {
            matches!(b, reflect_llm::ContentBlock::Text { text } if text.contains("Always answer in bullet points") && text.contains("<system-reminder>"))
        }),
        _ => false,
    });
    assert!(has_guidance, "注入的引导应随 prompt 进入首请求");
}

/// PreCompact deny:超阈值触发压缩时被拒 → 本轮无 ContextCompacted。
#[tokio::test]
async fn pre_compact_deny_skips_compaction() {
    let engine = Arc::new(HookEngine::new());
    engine.register(ShellHook::new(
        "no-compact",
        HookEventKind::PreCompact,
        None,
        "echo '{\"decision\":\"deny\",\"reason\":\"busy hour\"}'",
        std::time::Duration::from_secs(5),
    ));
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            trigger_tokens: 50, // 极低阈值:必然触发
            summarize_after: false,
            ..Default::default()
        },
        Arc::new(CannedSummarizer {
            text: String::new(),
            fail: true,
        }),
    ));
    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(vec![vec![
            ChatEvent::MessageStart {
                id: "m".into(),
                model: "scripted".into(),
            },
            ChatEvent::MessageStop,
        ]]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let thread = build(client, make_m4(compactor), Some(engine));

    let long_input = "历史背景。".repeat(120);
    let mut h = thread.submit(sub("k1", user_input(&long_input))).await;
    let deadline = Duration::from_secs(10);
    let mut saw_compacted = false;
    while let Some(ev) = timeout(deadline, h.next()).await.expect("event in time") {
        match ev.msg {
            EventMsg::ContextCompacted(c) => {
                saw_compacted =
                    !matches!(c.strategy, reflect_protocol::ContextCompactedStrategy::Noop);
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(!saw_compacted, "PreCompact deny 应跳过压缩(无真实压缩事件)");
}
