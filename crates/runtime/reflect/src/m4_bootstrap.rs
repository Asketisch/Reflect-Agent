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
/// - `thread_id` 用作 rollout 文件名 / session notes 文件名;
/// - `rollout_base` 为 `None` 时用默认 `~/.reflect/sessions`;`Some(dir)`
///   用自定义目录(SDK 消费者经 `ReflectBuilder::with_rollout_dir` 传入,
///   便于测试隔离 / 多租户分目录)。
pub fn build_default_m4(
    workspace: &std::path::Path,
    agent_name: &str,
    model: &str,
    registry: &reflect_llm::SharedModelRegistry,
    thread_id: ThreadId,
) -> anyhow::Result<M4Deps> {
    build_default_m4_with_prompts(workspace, agent_name, model, registry, thread_id, &[], None)
}

/// [`build_default_m4`] 的扩展版:额外扫描 `extra_prompt_dirs` 下的
/// `*.md` 模型层(v1.6,TUI / GUI 经 `ReflectBuilder::with_prompts_dir`
/// 传入)。目录顺序即优先级:后扫者覆盖同名层;固定顺序为
/// `<workspace>/.reflect/prompts` → `~/.reflect/prompts` → 额外目录。
///
/// v1.x:`rollout_base` 参数化 rollout 落盘根目录(见 [`build_default_m4`])。
pub fn build_default_m4_with_prompts(
    workspace: &std::path::Path,
    agent_name: &str,
    model: &str,
    registry: &reflect_llm::SharedModelRegistry,
    thread_id: ThreadId,
    extra_prompt_dirs: &[std::path::PathBuf],
    rollout_base: Option<&std::path::Path>,
) -> anyhow::Result<M4Deps> {
    let _ = model; // model spec 由 model_call 每请求解析,此处不再消费
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = std::path::Path::new(&home);

    // ── 提示词库(按模型匹配,v1.6)──────────────────────────
    let ws_prompts = workspace.join(".reflect").join("prompts");
    let home_prompts = home_path.join(".reflect").join("prompts");
    let mut prompt_dirs: Vec<std::path::PathBuf> = vec![home_prompts, ws_prompts];
    prompt_dirs.extend(extra_prompt_dirs.iter().cloned());
    let prompt_dir_refs: Vec<&std::path::Path> = prompt_dirs.iter().map(|p| p.as_path()).collect();
    reflect_prompt::init_library(&prompt_dir_refs, None);
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
            // v1.6:默认 agent 不再携带固定文案 —— 标记为内置默认,
            // pre_loop 按当前模型 spec 从提示词库解析 `system.main`。
            system_prompt: String::new(),
            source: reflect_agent_def::SystemPromptSource::BuiltinDefault,
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
    // v1.x:rollout_base 为 None 时用默认 `~/.reflect/sessions`;
    // Some(dir) 用自定义目录(SDK 消费者经 ReflectBuilder::with_rollout_dir
    // 传入)。完全关闭 rollout 由调用方在拿到 M4Deps 后置
    // `recorder = None`(ReflectBuilder::without_rollout)。
    let base = rollout_base
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_base);
    let recorder: Arc<dyn RolloutRecorder> = Arc::new(JsonlRolloutWriter::new(base, thread_id));

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
        prompt_dirs,
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

/// 库门面默认 agent system prompt 已外置(v1.6):短编码助手 5 条约束
/// 迁入提示词库 `system.facade-main` section(内嵌 `prompts/default.md`),
/// 运行时按当前模型 spec 解析 —— 可被 `.reflect/prompts/*.md` 覆盖。
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

    /// v1.x:自定义 rollout_base 时,recorder 把记录写到该目录而非默认
    /// `~/.reflect/sessions`(SDK 消费者经 `ReflectBuilder::with_rollout_dir`
    /// 传入,便于测试隔离 / 多租户分目录)。
    #[tokio::test]
    async fn build_default_m4_writes_rollout_to_custom_base() {
        let tmp = TempDir::new().expect("tempdir");
        let registry: reflect_llm::SharedModelRegistry =
            Arc::new(reflect_llm::ModelRegistry::new());
        let rollout_dir = tmp.path().join("custom-sessions");
        let thread_id = ThreadId::new();
        let m4 = build_default_m4_with_prompts(
            tmp.path(),
            "test-agent",
            "openai/gpt-4o",
            &registry,
            thread_id,
            &[],
            Some(&rollout_dir),
        )
        .expect("build_default_m4_with_prompts should succeed");
        let rec = m4.recorder.expect("默认应挂 recorder");
        // 写一条 SessionMeta,验证文件落在自定义目录下。
        rec.record(reflect_protocol::RolloutRecord::session_meta(
            thread_id,
            "openai/gpt-4o",
        ))
        .await
        .expect("record should succeed");
        // session_path_at 按当前 UTC 日期分桶;递归查找该 session 的文件,
        // 不依赖具体日期。
        let found = find_file_recursive(&rollout_dir, &format!("{thread_id}.jsonl"));
        assert!(
            found.is_some(),
            "rollout 文件应落在自定义 base {} 下",
            rollout_dir.display()
        );
    }

    /// 递归在 `dir` 下找名为 `name` 的文件(测试辅助)。
    fn find_file_recursive(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_file_recursive(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        }
        None
    }
}
