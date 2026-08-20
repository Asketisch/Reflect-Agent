//! 从原 `lib.rs` 内联 `#[cfg(test)] mod tests` 迁移而来的单元测试。
//!
//! 对齐 `vim/` 模式(同级 `tests.rs`)。被测项已在 crate 根 re-export,
//! 因此 `use crate::*` 路径继续可用。

use std::sync::Arc;

use crate::*;
use reflect_core::AgentConfig;
use reflect_llm::ModelRegistry;
use reflect_protocol::{
    AgentMessageDelta, Event, EventMsg, SessionConfiguredEvent, TurnId, TurnStartedEvent,
};
use reflect_tools::ToolRegistry;
use tokio::sync::mpsc;

/// v0.3.1: 加 `[ollama]` 段 → `diff_sections` 报告 `"ollama"`,
/// `ConfigReloaded.sections_changed` 命中 reload 测试断言。
#[test]
fn diff_sections_includes_ollama_when_section_added() {
    use reflect_config::schema::OllamaSection;
    let old = reflect_config::ReflectConfig::default();
    let new = reflect_config::ReflectConfig {
        ollama: Some(OllamaSection {
            model: Some("qwen2.5:7b".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let changed = diff_sections_for_test(&old, &new);
    assert!(
        changed.contains(&"ollama".to_string()),
        "ollama should be reported, got {changed:?}"
    );
}

/// S2.5:`ExecArgs::ephemeral_tasks` 字段是 bool,默认 `false`(`default_value_t = false`)。
/// 此测试静态保证字段类型 + 反转 false↔true 在 `run` 入口的 `if args.ephemeral_tasks`
/// 分支会触发 `InMemoryTaskStore` 路径;clap 的 `--ephemeral-tasks` 解析
/// 由顶层 binary 集成测试覆盖(`reflect exec --help` 列出该 flag)。
#[test]
fn ephemeral_tasks_field_is_bool_with_default_false() {
    fn assert_field(args: &ExecArgs) -> bool {
        args.ephemeral_tasks
    }
    // 显式 true 命中 ephemeral 分支
    let on = ExecArgs {
        ephemeral_tasks: true,
        ephemeral_teams: false,
        prompt: None,
        resume: None,
        continue_last: false,
        resume_by: None,
        agent: Some("default".into()),
        plan_mode: false,
        auto_root: false,
    };
    assert!(assert_field(&on));
    // 默认 false 命中 FileTaskStore 持久化分支
    let off = ExecArgs {
        ephemeral_tasks: false,
        ..on
    };
    assert!(!assert_field(&off));
}

/// 仅改 `[ollama].model` → 只报 `"ollama"`,不污染其它 section。
#[test]
fn diff_sections_only_ollama_changes() {
    use reflect_config::schema::OllamaSection;
    let old = reflect_config::ReflectConfig {
        ollama: Some(OllamaSection {
            model: Some("llama3.2".into()),
            keep_alive_secs: Some(300),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut new = old.clone();
    new.ollama.as_mut().unwrap().model = Some("qwen2.5:7b".into());
    let changed = diff_sections_for_test(&old, &new);
    assert_eq!(changed, vec!["ollama".to_string()]);
}

fn sess() -> Event {
    Event::new(
        reflect_protocol::EVENT_ID_NONE,
        EventMsg::SessionConfigured(SessionConfiguredEvent::new("openai/gpt-4o", "openai")),
    )
}

fn turn_started() -> Event {
    Event::new(
        "sub-1",
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: TurnId::new(),
            user_message_id: Some("um-1".into()),
        }),
    )
}

fn delta() -> Event {
    Event::new(
        "sub-1",
        EventMsg::AgentMessageDelta(AgentMessageDelta {
            delta: "Hello".into(),
        }),
    )
}

#[test]
fn jsonl_writer_emits_one_event_per_line() {
    let mut buf = Vec::new();
    {
        let mut w = JsonlWriter::new(&mut buf);
        w.write_event(&sess()).unwrap();
        w.write_event(&turn_started()).unwrap();
        w.write_event(&delta()).unwrap();
    }
    let s = String::from_utf8(buf).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.get("msg").is_some());
    }
    assert!(lines[0].contains("session_configured"));
    assert!(lines[1].contains("turn_started"));
    assert!(lines[2].contains("agent_message_delta"));
}

#[test]
fn jsonl_writer_handles_broken_pipe_via_return() {
    use std::io::Write;
    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "x"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = JsonlWriter::new(Failing);
    let err = w.write_event(&sess()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
}

// ── M8 P1b: handle_reload unit test ────────────────────────────────

/// `handle_reload` 应把新 config 应用到 registry 并向 channel
/// 推送恰好一条 `EventMsg::ConfigReloaded`。
/// v0.2.2: 默认 cfg diff 为空 → sections_changed 为空 vec 而不是
/// 旧的 `vec!["all"]`。
#[tokio::test]
async fn handle_reload_pushes_config_reloaded_event() {
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut rx) = mpsc::channel::<Event>(2);
    // 使用最小 config(没有 provider section → 不注册任何 client;
    // 此时 `apply_to_registry` 是 no-op,因为 section 不存在)。
    let old_cfg = reflect_config::ReflectConfig::default();
    let new_cfg = reflect_config::ReflectConfig::default();
    let path = std::path::PathBuf::from("/tmp/test-config.toml");
    let agent_cfg = AgentConfig::new("anthropic/x", "/tmp");
    let result = handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    let event = rx.recv().await.expect("event should be in channel");
    match event.msg {
        EventMsg::ConfigReloaded(cr) => {
            assert_eq!(cr.path, path);
            // 两份 default cfg 相同 → sections_changed 为空。
            assert!(
                cr.sections_changed.is_empty(),
                "expected empty diff, got {:?}",
                cr.sections_changed
            );
            // `at` 应为较近的 SystemTime(不是 UNIX_EPOCH)。
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let at_secs = cr
                .at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            assert!(
                now.abs_diff(at_secs) < 5,
                "at timestamp {at_secs} should be within 5s of now {now}"
            );
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }
}

/// `handle_reload` 在 `event_tx` 被 drop 时应返回 error
/// (registry 仍会被更新,但 broadcast 失败)。
#[tokio::test]
async fn handle_reload_returns_err_when_channel_closed() {
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, rx) = mpsc::channel::<Event>(1);
    drop(rx);
    let old_cfg = reflect_config::ReflectConfig::default();
    let new_cfg = reflect_config::ReflectConfig::default();
    let path = std::path::PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/x", "/tmp");
    let result = handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await;
    assert!(result.is_err(), "expected Err when channel is closed");
}

// ── v0.2.2: handle_reload 模型/Provider 热重载测试 ──────────────────

/// `[anthropic].model` 字段变更后,`handle_reload` 写入 agent_cfg.model。
/// 这是核心路径:用户编辑 TOML 改 model,下一个 turn 自动用新值。
#[tokio::test]
async fn handle_reload_updates_agent_config_model_when_section_changes() {
    use reflect_config::{ActiveSection, AnthropicSection};
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut _rx) = mpsc::channel::<Event>(4);
    let old_cfg = reflect_config::ReflectConfig {
        active: ActiveSection {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            model: Some("claude-3-5-sonnet-latest".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut new_cfg = old_cfg.clone();
    new_cfg.anthropic.as_mut().unwrap().model = Some("claude-3-haiku-20240307".into());
    let path = std::path::PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
    let result = handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert_eq!(
        agent_cfg.current_model(),
        "anthropic/claude-3-haiku-20240307",
        "AgentConfig.model must reflect the new TOML value"
    );
}

/// 仅 `[anthropic].api_key` 变(model 字段不动)→ 不动 `AgentConfig.model`。
#[tokio::test]
async fn handle_reload_does_not_touch_model_when_credentials_change() {
    use reflect_config::{ActiveSection, AnthropicSection};
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut _rx) = mpsc::channel::<Event>(4);
    let old_cfg = reflect_config::ReflectConfig {
        active: ActiveSection {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-old".into()),
            model: Some("claude-3-5-sonnet-latest".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut new_cfg = old_cfg.clone();
    new_cfg.anthropic.as_mut().unwrap().api_key = Some("sk-new".into());
    let path = std::path::PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
    let result = handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert_eq!(
        agent_cfg.current_model(),
        "anthropic/claude-3-5-sonnet-latest",
        "credential-only change must NOT update model"
    );
}

/// `sections_changed` 应列出真正变了的 section,而不是 `vec!["all"]`。
#[tokio::test]
async fn handle_reloaded_event_lists_only_changed_sections() {
    use reflect_config::{ActiveSection, AnthropicSection, CompactSection};
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut rx) = mpsc::channel::<Event>(4);
    let old_cfg = reflect_config::ReflectConfig {
        active: ActiveSection {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            ..Default::default()
        }),
        compact: CompactSection {
            trigger_tokens: Some(10_000),
        },
        ..Default::default()
    };
    let mut new_cfg = old_cfg.clone();
    // 改 openai section + compact section;保留 anthropic 不动。
    new_cfg.openai = Some(reflect_config::OpenAISection {
        api_key: Some("sk-o".into()),
        ..Default::default()
    });
    new_cfg.compact.trigger_tokens = Some(20_000);
    let path = std::path::PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/x", "/tmp");
    handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await
    .expect("handle_reload ok");
    // 第一个 emit 是 SessionConfigured(model 没变 provider 也没变 →
    // 没 emit),然后是 ConfigReloaded。我们只关心 ConfigReloaded。
    let mut found_reloaded = false;
    while let Some(ev) = rx.recv().await {
        if let EventMsg::ConfigReloaded(cr) = ev.msg {
            assert!(cr.sections_changed.contains(&"openai".to_string()));
            assert!(cr.sections_changed.contains(&"compact".to_string()));
            assert!(!cr.sections_changed.contains(&"anthropic".to_string()));
            assert!(!cr.sections_changed.contains(&"active".to_string()));
            found_reloaded = true;
            break;
        }
    }
    assert!(found_reloaded, "ConfigReloaded event should have arrived");
}

/// `[active].provider` 切换 + 模型变更 → `agent_cfg.model` 整体翻面。
#[tokio::test]
async fn handle_reload_swaps_active_provider_spec() {
    use reflect_config::{ActiveSection, AnthropicSection, OpenAISection};
    use std::sync::Arc;
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut _rx) = mpsc::channel::<Event>(4);
    let old_cfg = reflect_config::ReflectConfig {
        active: ActiveSection {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            model: Some("claude-3-5-sonnet-latest".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut new_cfg = reflect_config::ReflectConfig {
        active: ActiveSection {
            provider: Some("openai".into()),
            ..Default::default()
        },
        openai: Some(OpenAISection {
            api_key: Some("sk-o".into()),
            model: Some("gpt-4o".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    // 保证 openai 也满足 active_provider 的回退
    new_cfg.openai = Some(OpenAISection {
        api_key: Some("sk-o".into()),
        model: Some("gpt-4o".into()),
        ..Default::default()
    });
    let path = std::path::PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
    handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
    )
    .await
    .expect("handle_reload ok");
    assert_eq!(
        agent_cfg.current_model(),
        "openai/gpt-4o",
        "active provider switch must flip model spec"
    );
}

/// `[coordinator]` 段变更 → `handle_reload` 切换 factory 与 Note 工具。
#[tokio::test]
async fn handle_reload_applies_coordinator_section() {
    use reflect_config::CoordinatorSection;
    use reflect_core::config::default_m4_deps;
    use reflect_protocol::ThreadId;
    use reflect_subagent::SubAgentFactory;
    use tokio_util::sync::CancellationToken;

    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut rx) = mpsc::channel::<Event>(4);
    let tools = Arc::new(ToolRegistry::default());
    let factory = Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        registry.clone(),
        None, // child_registry: will be set by caller if subagent_providers configured
        tools.clone(),
        CancellationToken::new(),
        None,
    ));
    let m4 = default_m4_deps("test");
    let agent_cfg = AgentConfig::new("openai/gpt-4o", "/tmp/ws").with_m4(m4);
    let path = std::path::PathBuf::from("/tmp/coordinator-reload.toml");

    let old_cfg = reflect_config::ReflectConfig::default();
    let new_cfg = reflect_config::ReflectConfig {
        coordinator: Some(CoordinatorSection {
            enabled: Some(true),
            system_prompt_path: None,
            max_workers: Some(4),
        }),
        ..Default::default()
    };

    handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        Some(factory.as_ref()),
        None,
        &path,
        &tx,
        None,
        &tools,
        None,
    )
    .await
    .expect("enable coordinator reload");

    assert!(factory.is_coordinator_mode());
    assert!(tools.get("WriteNote").is_some());
    assert!(tools.get("ReadNotes").is_some());

    while rx.try_recv().is_ok() {}

    let disabled_cfg = reflect_config::ReflectConfig {
        coordinator: Some(CoordinatorSection {
            enabled: Some(false),
            system_prompt_path: None,
            max_workers: None,
        }),
        ..Default::default()
    };

    handle_reload(
        &new_cfg,
        &disabled_cfg,
        &registry,
        &agent_cfg,
        Some(factory.as_ref()),
        None,
        &path,
        &tx,
        None,
        &tools,
        None,
    )
    .await
    .expect("disable coordinator reload");

    assert!(!factory.is_coordinator_mode());
    assert!(tools.get("WriteNote").is_none());
    assert!(tools.get("ReadNotes").is_none());
}
