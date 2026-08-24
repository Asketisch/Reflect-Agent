//! 库门面 M4 依赖接线。
//!
//! 把 `reflect-exec::bootstrap_m4` 的核心逻辑下沉一份到库 crate,
//! 让 `ReflectBuilder::with_defaults()` 在不依赖 `reflect-exec` 私有
//! helper 的情况下,也能拿到一份完整的 M4 依赖(compactor / memory /
//! skills / prompt builder / agent def / recorder / note store / file
//! recovery / subagent registry)。
//!
//! 与 `reflect-exec` 路径差异(精简,只满足 facade 默认场景):
//! - 不挂 LSP / MCP(那是 `reflect-exec::bootstrap_m6` 的职责);
//! - coordinator mode 通过环境变量 `REFLECT_COORDINATOR_MODE=1` 启用,
//!   与 exec 路径对齐;
//! - session notes 落盘失败 → fallback `InMemoryNoteStore`,不阻塞启动;
//! - recorder 默认挂 `JsonlRolloutWriter` 到 `~/.reflect/sessions/`,
//!   与 exec 一致。

use std::sync::Arc;

use parking_lot::Mutex;
use reflect_agent_def::AgentDefinition;
use reflect_compact::{Compactor, LlmSummarizer, Summarizer};
use reflect_core::config::{M4Deps, compactor_config_from_env_and_toml, default_m4_deps};
use reflect_llm::RoutingPolicy;
use reflect_memory::{FileMemoryStore, InMemoryStore, MemoryStore};
use reflect_notes::NoteStore;
use reflect_prompt::PromptBuilder;
use reflect_protocol::{RolloutRecorder, ThreadId};
use reflect_recovery::{ActiveFileRecovery, SubagentRegistry};
use reflect_rollout::{JsonlRolloutWriter, path::default_base};
use reflect_skills::SkillsCatalog;
use reflect_task::coordinator::CoordinatorConfig;

/// 在库门面构造一份"默认够用"的 M4Deps。
///
/// - `workspace` 决定相对路径与 skills 扫描位置;
/// - `agent_name` 在 workspace + home agents 目录中查找同名 agent 定义,
///   找不到时回退默认 system prompt(简短编码助手,5 条约束);
/// - `model` 作为 LLM summarizer 的 fallback model spec;
/// - `registry` 必须包含至少 1 个 provider pool,否则 summarizer 退化为 Noop;
/// - `thread_id` 用作 rollout 文件名 / session notes 文件名。
pub fn build_default_m4(
    workspace: &std::path::Path,
    agent_name: &str,
    model: &str,
    registry: &reflect_llm::SharedModelRegistry,
    thread_id: ThreadId,
) -> anyhow::Result<M4Deps> {
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = std::path::Path::new(&home);

    // ── Agent 定义 ────────────────────────────────────────────
    let ws_agents = workspace.join(".reflect").join("agents");
    let home_agents = home_path.join(".reflect").join("agents");
    let mut agents = reflect_agent_def::load_agents_dir(&ws_agents).unwrap_or_default();
    let home_agents_map = reflect_agent_def::load_agents_dir(&home_agents).unwrap_or_default();
    agents.extend(home_agents_map);
    let active_def = agents
        .remove(agent_name)
        .unwrap_or_else(|| AgentDefinition {
            name: agent_name.to_string(),
            description: "default agent".into(),
            system_prompt: DEFAULT_FACADE_SYSTEM_PROMPT.into(),
            ..Default::default()
        });
    let active_def = Arc::new(active_def);

    // ── Skills ────────────────────────────────────────────────
    let ws_skills = workspace.join(".reflect").join("skills");
    let home_skills = home_path.join(".reflect").join("skills");
    let skill_dirs: Vec<&std::path::Path> = vec![&ws_skills, &home_skills];
    let skills_catalog = SkillsCatalog::new();
    skills_catalog.scan(&skill_dirs);
    reflect_skills::merge_bundled(&skills_catalog);
    let skills_catalog = Arc::new(skills_catalog);

    // ── Memory:CompositeMemoryStore(Session 内存 + Project/User 文件) ──
    let file_store = Arc::new(FileMemoryStore::new(workspace, home_path));
    let memory: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::with_fallback(file_store));

    // ── Session notes(落盘失败 → 内存 fallback) ──────────────
    let note_store: Arc<dyn NoteStore> = match build_note_store(thread_id) {
        Some(store) => store,
        None => Arc::new(reflect_notes::InMemoryNoteStore::new()),
    };

    // ── Compactor(LlmSummarizer 优先,无 provider 退化 Noop) ──
    let cfg = reflect_config::load_default();
    let policy = cfg.routing_policy();
    let summarizer: Arc<dyn Summarizer> = {
        let spec = {
            let p = policy.resolve(reflect_llm::Role::Compact).primary.clone();
            if p.is_empty() { model.to_string() } else { p }
        };
        if registry.next_for(&spec, &[]).is_some() {
            Arc::new(LlmSummarizer::new(
                registry.clone(),
                Arc::new(RoutingPolicy::default()),
                spec,
            ))
        } else {
            Arc::new(NoopSummarizer)
        }
    };
    let compactor_cfg = compactor_config_from_env_and_toml(None);
    let compactor = Arc::new(Compactor::new(compactor_cfg, summarizer));

    // ── Prompt builder(coordinator mode 注入对应 section) ────
    let prompt_builder = Arc::new(Mutex::new(PromptBuilder::new()));
    let coord_cfg = CoordinatorConfig::from_env_or_config(
        cfg.coordinator
            .as_ref()
            .unwrap_or(&reflect_config::CoordinatorSection::default()),
    );
    if coord_cfg.enabled {
        prompt_builder
            .lock()
            .upsert_section("Coordinator", coord_cfg.system_prompt.clone());
    }

    // ── Recorder:默认挂 JsonlRolloutWriter ──────────────────
    let recorder: Arc<dyn RolloutRecorder> =
        Arc::new(JsonlRolloutWriter::new(default_base(), thread_id));

    // ── file recovery + subagent registry ────────────────────
    let file_recovery = Arc::new(ActiveFileRecovery::new(Arc::from(workspace.to_path_buf())));
    let subagent_registry = SubagentRegistry::shared();

    Ok(M4Deps {
        compactor,
        memory,
        skills: skills_catalog,
        prompt_builder,
        active_agent_def: active_def,
        recorder: Some(recorder),
        note_store,
        file_recovery,
        subagent_registry,
    })
}

/// 在无法走完整 bootstrap(没有 LLM provider、纯单测场景)时,
/// 返回一份不带 compactor / 不带 LLM summarizer 的最小 M4Deps。
/// 用于 `with_defaults` 内部"软退化"路径。
#[allow(dead_code)]
pub fn build_minimal_m4(agent_name: &str) -> M4Deps {
    default_m4_deps(agent_name)
}

fn build_note_store(thread_id: ThreadId) -> Option<Arc<dyn NoteStore>> {
    let home = reflect_notes::resolve_notes_home()?;
    let dir = home.join("session-notes");
    let path = dir.join(format!("{}.jsonl", thread_id));
    match reflect_notes::FileBackedNoteStore::open_or_create_default(&path) {
        Ok(s) => Some(Arc::new(s)),
        Err(_) => None,
    }
}

/// Noop summarizer:无 LLM provider 时让 compactor 仍能工作(降级)。
struct NoopSummarizer;

#[async_trait::async_trait]
impl Summarizer for NoopSummarizer {
    async fn summarize_full(
        &self,
        _msgs: &[reflect_llm::ChatMessage],
    ) -> Result<String, reflect_compact::SummarizerError> {
        Err(reflect_compact::SummarizerError::Cancelled)
    }
    async fn summarize_recent(
        &self,
        _msgs: &[reflect_llm::ChatMessage],
        _prev: Option<&str>,
    ) -> Result<String, reflect_compact::SummarizerError> {
        Err(reflect_compact::SummarizerError::Cancelled)
    }
}

/// 库门面默认 agent system prompt —— 短编码助手 5 条约束。
/// 与 `reflect-exec` bootstrap 用的 `DEFAULT_SYSTEM_PROMPT` 语义一致,
/// 但更短(facade 用户通常做最小集成,不需要冗长行为约束)。
const DEFAULT_FACADE_SYSTEM_PROMPT: &str = "你是一名编码助手。遵循以下约定:\
1. 先读后改;2. 不做过度设计;3. 注释只解释「为什么」;\
4. 破坏性操作前确认;5. 用与用户相同的语言回答。";

// ── 单元测试 ────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::ThreadId;
    use tempfile::TempDir;

    #[test]
    fn build_default_m4_attaches_every_required_field() {
        let tmp = TempDir::new().expect("tempdir");
        let registry: reflect_llm::SharedModelRegistry =
            Arc::new(reflect_llm::ModelRegistry::new());
        let m4 = build_default_m4(
            tmp.path(),
            "test-agent",
            "openai/gpt-4o",
            &registry,
            ThreadId::new(),
        )
        .expect("build_default_m4 should succeed");
        // 断言每个字段都已填(关键——防止回归回退到占位实现)
        assert!(Arc::strong_count(&m4.compactor) >= 1);
        assert_eq!(m4.active_agent_def.name, "test-agent");
        assert!(m4.recorder.is_some(), "默认应挂 JsonlRolloutWriter");
    }

    #[test]
    fn build_minimal_m4_uses_agent_name() {
        let m4 = build_minimal_m4("named-agent");
        assert_eq!(m4.active_agent_def.name, "named-agent");
    }
}
