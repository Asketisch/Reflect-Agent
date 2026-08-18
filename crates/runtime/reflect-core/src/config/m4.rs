//! `M4Deps` — M4 生产依赖的合集(compactor、memory、skills、prompt builder、
//! agent def、recorder、note store、file recovery、subagent registry)以及
//! `default_m4_deps` 测试辅助。

use std::sync::Arc;

use parking_lot::Mutex;
use reflect_agent_def::AgentDefinition;
use reflect_compact::Compactor;
use reflect_memory::{FileMemoryStore, MemoryStore};
use reflect_notes::NoteStore;
use reflect_prompt::PromptBuilder;
use reflect_protocol::RolloutRecorder;
use reflect_recovery::{ActiveFileRecovery, SubagentRegistry};
use reflect_skills::SkillsCatalog;

/// M4 依赖合集。每个字段独立可选,生产 bootstrap 可渐进填入。
#[derive(Clone)]
pub struct M4Deps {
    pub compactor: Arc<Compactor>,
    pub memory: Arc<dyn MemoryStore>,
    pub skills: Arc<SkillsCatalog>,
    pub prompt_builder: Arc<Mutex<PromptBuilder>>,
    pub active_agent_def: Arc<AgentDefinition>,
    /// M5:[`RolloutRecord`] 的可选持久化 sink。测试中为 `None`;
    /// `reflect-exec` 启动时接入 `JsonlRolloutWriter`。
    pub recorder: Option<Arc<dyn RolloutRecorder>>,
    /// v1.1.0 Phase 6 P0:Session memory 笔记存储(FIFO 30 + JSONL
    /// 落盘)。`pre_loop` 调 `as_meta_message()` 把当前队列渲染成
    /// `<system-reminder>` 注入到 LLM。
    pub note_store: Arc<dyn NoteStore>,
    /// v1.1.0 Phase 6 P0:post-compact 活跃文件恢复。`pre_loop` 在压缩
    /// 触发后调 `recover(&messages)`,把最近 write / edit 过的文件
    /// 内容(50k token 预算,10 文件上限)注入到 `<system-reminder>`。
    pub file_recovery: Arc<ActiveFileRecovery>,
    /// v1.1.0 Phase 6 P0:已完成子代理调用注册表。`CallSubAgentTool`
    /// 写,`pre_loop` 读 + 渲染为 `[已完成的子代理调用记录]` 防止 LLM
    /// 重复 spawn。FIFO cap=32,跨 turn 共享在 `M4Deps` 上。
    pub subagent_registry: Arc<SubagentRegistry>,
}

impl std::fmt::Debug for M4Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("M4Deps")
            .field("compactor", &"<dyn Compactor>")
            .field("memory", &"<dyn MemoryStore>")
            .field("skills", &self.skills)
            .field("prompt_builder", &"<PromptBuilder>")
            .field("active_agent_def", &self.active_agent_def.name)
            .field(
                "recorder",
                &self.recorder.as_ref().map(|_| "<dyn RolloutRecorder>"),
            )
            .field("note_store", &"<dyn NoteStore>")
            .field("file_recovery", &self.file_recovery)
            .field("subagent_registry", &self.subagent_registry)
            .finish()
    }
}

/// 测试辅助:按给定 agent 名构造一个 no-op `M4Deps`。
/// 使用空 `Compactor`(no-op)+ 空 `FileMemoryStore`(写入 tempdir 或
/// scratch 目录)+ 空 `SkillsCatalog` + 默认 `PromptBuilder` +
/// 默认 `AgentDefinition`。
pub fn default_m4_deps(agent_name: &str) -> M4Deps {
    use reflect_compact::{CompactorConfig, Summarizer};
    use std::sync::Arc as StdArc;
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
    let compactor = Arc::new(Compactor::new(
        CompactorConfig {
            summarize_after: false, // 测试不希望发生真实 LLM 调用
            ..Default::default()
        },
        StdArc::new(NoopSummarizer),
    ));
    // 用唯一 tempdir 存 memory,避免测试间相互污染。
    let tmp = std::env::temp_dir().join(format!("reflect-test-mem-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&tmp);
    let memory: Arc<dyn MemoryStore> = Arc::new(FileMemoryStore::new(&tmp, &tmp));
    let skills = Arc::new(SkillsCatalog::new());
    let prompt_builder = Arc::new(Mutex::new(PromptBuilder::new()));
    let note_store: Arc<dyn reflect_notes::NoteStore> =
        Arc::new(reflect_notes::InMemoryNoteStore::new());
    let file_recovery = Arc::new(reflect_recovery::ActiveFileRecovery::new(Arc::from(tmp)));
    let subagent_registry = reflect_recovery::SubagentRegistry::shared();
    let mut def = AgentDefinition::default();
    #[allow(clippy::field_reassign_with_default)] // pre-M5: clearer than struct-literal
    {
        def.name = agent_name.to_string();
        def.description = format!("test agent {agent_name}");
        def.system_prompt = "You are a test agent.".into();
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
