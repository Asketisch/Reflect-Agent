//! Session 引导:装配 M4(agent 定义 / 记忆 / 技能 / 压缩器)、
//! M5(子代理工厂)、M6(MCP)、LSP,以及 resume 回放。
//!
//! 从 `lib.rs` 原样抽出。这些是较重的启动函数,由 `async_main` 的
//! 正常分支与 resume 分支调用。

use std::sync::Arc;

use parking_lot::Mutex;
use reflect_agent_def::AgentDefinition;
use reflect_compact::{Compactor, LlmSummarizer, Summarizer, SummarizerError};
use reflect_llm::{ChatMessage, SharedModelRegistry};
use reflect_lsp::{LspConnectionManager, LspLifecycleEvent, LspTool};
use reflect_mcp::{McpConnectionManager, McpLifecycleEvent, McpServerConfig, McpToolAdapter};
use reflect_memory::{FileMemoryStore, InMemoryStore, MemoryStore};
use reflect_prompt::PromptBuilder;
use reflect_protocol::ThreadId;
use reflect_rollout::JsonlRolloutWriter;
use reflect_skills::SkillsCatalog;
use reflect_subagent::{CallSubAgentTool, SubAgentFactory};
use reflect_task::coordinator::CoordinatorConfig;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

// ── Thread-local 工具注册表句柄 ───────────────────────────────────────

// M4 bootstrap 用的 thread-local tool registry 句柄。`async_main`
// 在调用 `bootstrap_m4` 前设置之。
thread_local! {
    pub(crate) static TOOLS: std::cell::RefCell<Option<Arc<ToolRegistry>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn tools() -> Arc<ToolRegistry> {
    TOOLS.with(|t| t.borrow().clone().expect("tools not initialized"))
}

// ── M4 引导 ────────────────────────────────────────────────────────────

/// 默认 agent 系统提示(用户未提供 `.reflect/agents/<name>.md` 时的 fallback)。
///
/// 只覆盖 5 条最通用的编码 agent 行为约束,刻意保持简短:具体能力由工具
/// 和 skills 在运行时叠加。借鉴业界通用编码 agent 的安全执行实践
/// (如「做任务不画蛇添足」「注释解释为什么」等成熟约定),但不绑定技术栈、
/// 不含外部链接 / 模型身份 / 区域限制等不适用条款。
const DEFAULT_SYSTEM_PROMPT: &str = "你是一名编码助手。在回答用户请求时遵守以下约定:\n\
\n\
1. 先读后改:编辑文件前先用 `read` 读目标文件;做有针对性的修改,而非整文件重写。\n\
2. 先约定后实现:不要假设库或工具可用。从 README、package manifest(如 \
Cargo.toml / package.json)、邻近文件确认约定与风格,模仿现有代码的命名与惯用法。\n\
3. 不过度设计:只做被要求的事,不主动增加范围外的功能、重构或抽象。三行重复 \
优于过早抽象。任务完成后不要顺手创建未要求的 README / 测试 / 文档。\n\
4. 注释克制:除非用户要求,不要写注释;需要写注释时,只解释「为什么」, \
不要复述代码「是什么」。\n\
5. 破坏性操作前确认:对难以撤销或对外可见的操作,先说明再执行。典型例子包括 \
`rm -rf`、`git reset --hard`、删除分支、`git push`(含 `--force`)、 \
发送外部请求或提交等。\n\
\n\
语言:用与用户提问相同的语言回答。";

/// v1.1.0 Phase 6 P0:在 `$REFLECT_HOME/session-notes/<thread_id>.jsonl`
/// 上构造 `FileBackedNoteStore`。任何一步失败都返回 `None` —— caller
/// (`bootstrap_m4`) 会 fallback 到纯 `InMemoryNoteStore`,保证启动不
/// 因 note 落盘失败而阻塞。
fn build_note_store(
    thread_id: reflect_protocol::ThreadId,
) -> Option<Arc<dyn reflect_notes::NoteStore>> {
    let home = reflect_notes::resolve_notes_home()?;
    let dir = home.join("session-notes");
    let path = dir.join(format!("{}.jsonl", thread_id));
    match reflect_notes::FileBackedNoteStore::open_or_create_default(&path) {
        Ok(s) => {
            tracing::info!(
                thread_id = %thread_id,
                path = %path.display(),
                "session notes 落盘已启用"
            );
            Some(Arc::new(s))
        }
        Err(e) => {
            tracing::warn!(
                thread_id = %thread_id,
                path = %path.display(),
                error = %e,
                "session notes JSONL 初始化失败;fallback 纯 RAM"
            );
            None
        }
    }
}

/// 为当前 session 构造 M4 依赖项:agent 定义、记忆、技能、压缩器、
/// prompt 构建器。
#[allow(clippy::too_many_arguments)]
pub(crate) fn bootstrap_m4(
    workspace: &std::path::Path,
    agent_name: &str,
    model: &str,
    registry: &SharedModelRegistry,
    thread_id: ThreadId,
    toml_trigger_tokens: Option<u32>,
    cfg: &reflect_config::ReflectConfig,
    telemetry: Option<Arc<reflect_telemetry::TelemetrySink>>,
) -> Option<reflect_core::config::M4Deps> {
    // Agent 定义:从 workspace + home agent 目录加载。
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = std::path::Path::new(&home);
    let ws_agents = workspace.join(".reflect").join("agents");
    let home_agents = home_path.join(".reflect").join("agents");
    let mut agents = reflect_agent_def::load_agents_dir(&ws_agents).unwrap_or_default();
    let home_agents_map = reflect_agent_def::load_agents_dir(&home_agents).unwrap_or_default();
    agents.extend(home_agents_map);
    let active_def = agents.remove(agent_name).unwrap_or_else(|| {
        #[allow(clippy::field_reassign_with_default)] // pre-M5
        {
            let mut d = AgentDefinition::default();
            d.name = agent_name.to_string();
            d.description = "default agent".into();
            d.system_prompt = DEFAULT_SYSTEM_PROMPT.into();
            d
        }
    });
    let active_def = Arc::new(active_def);

    // Skills:扫描 workspace + home skill 目录。
    let ws_skills = workspace.join(".reflect").join("skills");
    let home_skills = home_path.join(".reflect").join("skills");
    let skill_dirs: Vec<&std::path::Path> = vec![&ws_skills, &home_skills];
    let skills_catalog = SkillsCatalog::new();
    skills_catalog.scan(&skill_dirs);
    // v1.x:合并内置 skill 包(code-review / commit-helper 等,编译期嵌入)。
    // 此前 `merge_bundled` 是孤儿 —— 实现完整但从未调用,导致 15+ 内置
    // skill 对用户不可见。同名 skill 以文件目录扫描结果优先(`merge_bundled`
    // 内部 `get().is_none()` 守卫),用户自定义覆盖内置。
    reflect_skills::merge_bundled(&skills_catalog);
    let skills_catalog = Arc::new(skills_catalog);

    // 注册 `load_skill` 工具。
    // v1.3:走 `register_runtime_tool`,触发 Runtime 源的 `Prompt` 安全
    // floor。即使 `load_skill` 自身声明 `Auto`,外部 source 也会被
    // `FloorEnforcingTool` 包装抬升到 `Prompt`,避免被 plugin / MCP
    // 利用做静态放行。
    let load_skill = Arc::new(reflect_skills::LoadSkillTool::new(skills_catalog.clone()));
    tools().register_runtime_tool(load_skill);

    // Memory:project + user 走文件,session 走内存。
    // v1.x:接线 `CompositeMemoryStore`(此前孤儿)—— Session scope 走内存
    // (进程退出即丢,对话历史已在 rollout JSONL),Project/User scope 走文件。
    // 此前用纯 FileMemoryStore,Session scope 也落文件,与设计意图不符。
    let file_store = Arc::new(FileMemoryStore::new(workspace, home_path));
    let memory_arc: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::with_fallback(file_store));
    let memory: Arc<dyn MemoryStore> = memory_arc;

    // v1.1.0 Phase 6 P0:Session memory notes。
    //
    // 路径 = `$REFLECT_HOME/session-notes/<thread_id>.jsonl`,落盘失败
    // 时 fallback 纯 RAM store(不阻塞启动)。`AddSessionNoteTool` 走
    // 同一 Arc,工具调用写入和 pre_loop 注入读到同一份 FIFO 队列。
    let note_store: Arc<dyn reflect_notes::NoteStore> = match build_note_store(thread_id) {
        Some(store) => store,
        None => Arc::new(reflect_notes::InMemoryNoteStore::new()),
    };
    let note_tool: Arc<dyn reflect_tools::Tool> =
        Arc::new(reflect_notes::AddSessionNoteTool::new(note_store.clone()));
    // v1.3:走 `register_runtime_tool` 应用 Runtime 源安全 floor。
    tools().register_runtime_tool(note_tool);

    // v1.1.0 Phase 6 P0:Active File Recovery。post-compact 扫最近
    // write / edit 工具调用,重读文件内容注入 `<system-reminder>`。
    // workspace 走当前 session 的 workspace 绝对路径(与 `MemoryStore`
    // 一致)。`ActiveFileRecovery` 自身无 I/O 副作用,构造不会失败。
    let file_recovery = Arc::new(reflect_recovery::ActiveFileRecovery::new(Arc::from(
        workspace.to_path_buf(),
    )));

    // v1.1.0 Phase 6 P0:子代理调用注册表。`bootstrap_m5` 会把同一
    // Arc 注入 `SubAgentFactory`,`CallSubAgentTool::execute` 写,
    // `pre_loop` 读。纯 RAM,不落盘(子代理 session 结束即清理)。
    let subagent_registry = reflect_recovery::SubagentRegistry::shared();

    // Compactor:在有 LLM 客户端时接上 LLM 摘要器。
    // v1.0 多 Provider 路由:用 registry + policy 注入,`LlmSummarizer` 内
    // 部走 `Role::Compact` slot 并在失败时跨 credential failover。
    let summarizer: Arc<dyn Summarizer> = {
        let policy = cfg.routing_policy();
        // 选 compact slot 的 primary 作 spec 起点(向后兼容 model)
        let spec = {
            let p = policy.resolve(reflect_llm::Role::Compact).primary.clone();
            if p.is_empty() { model.to_string() } else { p }
        };
        if registry.next_for(&spec, &[]).is_some() {
            Arc::new(
                LlmSummarizer::new(registry.clone(), Arc::new(policy), spec)
                    .with_telemetry(telemetry.clone()),
            )
        } else {
            Arc::new(NoopSummarizer)
        }
    };
    // M8 P0b:触发阈值取自环境变量 `REFLECT_AUTO_COMPACT_INPUT_TOKENS`
    // > TOML `[compact].trigger_tokens` > 默认值 (10_000)。M7 仅记录了
    // 环境变量;M8 把两者都接入 `CompactorConfig`。
    let compactor_cfg =
        reflect_core::config::compactor_config_from_env_and_toml(toml_trigger_tokens);
    let compactor = Arc::new(Compactor::new(compactor_cfg, summarizer));

    // Prompt 构建器。
    // v1.1.0 Phase 4:若 `CoordinatorConfig::enabled`,把 coordinator
    // system prompt 作为命名 section 追加到 prompt_builder;`build_request`
    // 阶段会把 section 拼到 core 末尾,见 `reflect-prompt::builder::add_section`。
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
        tracing::info!(
            max_workers = coord_cfg.max_workers,
            "coordinator mode enabled; system prompt 注入到 prompt_builder"
        );
    }

    Some(reflect_core::config::M4Deps {
        compactor,
        memory,
        skills: skills_catalog,
        prompt_builder,
        active_agent_def: active_def,
        recorder: Some(Arc::new(JsonlRolloutWriter::new(
            reflect_rollout::path::default_base(),
            thread_id,
        ))),
        note_store,
        file_recovery,
        subagent_registry,
    })
}

/// M4 bootstrap 用的 thread-local tool registry 句柄。
///
/// 在 M4 之上构建 M5 层面:一个以当前 thread 为根的 `SubAgentFactory`,
/// 以及在父 `ToolRegistry` 上为每个声明的 subagent spec 注册一个
/// `call_<role>` 工具。
///
/// v0 提供两个硬编码 spec slot,用户将来可通过
/// `~/.reflect/subagents/*.md` 覆盖;现阶段只接入一个 `explorer` spec
/// 以端到端跑通路径。
///
/// v0.2.2: 返回 `Arc<SubAgentFactory>` —— `async_main` 需要把它交给
/// `spawn_reload_task`,以便热重载检测到 model 变更时同步更新
/// 调用 `factory.set_default_model()` 设定子代理默认模型。
#[allow(clippy::too_many_arguments)]
pub(crate) fn bootstrap_m5(
    workspace: &std::path::Path,
    agent_name: &str,
    model: &str,
    registry: &SharedModelRegistry,
    thread_id: ThreadId,
    parent_recorder: Arc<dyn reflect_protocol::RolloutRecorder>,
    m4: &reflect_core::config::M4Deps,
    initial_cfg: &reflect_config::ReflectConfig,
) -> Arc<SubAgentFactory> {
    // v1.x 功能 1:从 `[subagent_providers]` 段构建独立 child registry,
    // 让 subagent 用独立的 base_url + api_key + model。`None` = 父子共享。
    let child_registry: Option<SharedModelRegistry> =
        initial_cfg.to_child_registry().map(|reg| Arc::new(reg));
    let factory = Arc::new(SubAgentFactory::new(
        thread_id,
        model.to_string(),
        registry.clone(),
        child_registry,
        tools(),
        CancellationToken::new(),
        Some(parent_recorder),
    ));
    // v1.1.0 Phase 6 P0:把 `M4Deps` 里的 subagent registry 注入 factory,
    // 后续 `CallSubAgentTool::execute` 写 + `pre_loop` 读走同一 Arc。
    factory.set_subagent_registry(m4.subagent_registry.clone());
    // v1.x 功能 6:注入父 skills catalog,让 subagent 也能 LoadSkill / 使用 skill。
    factory.set_parent_skills(m4.skills.clone());

    // v1.x 功能 2:合并 subagent spec —— 优先级
    // TOML `[[subagents]]` > workspace `.reflect/subagents/*.md`
    // > home `~/.reflect/subagents/*.md` > 硬编码 explorer(仅当全部为空时)。
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = std::path::Path::new(&home);
    let ws_subagents = workspace.join(".reflect").join("subagents");
    let home_subagents = home_path.join(".reflect").join("subagents");
    let home_md = reflect_subagent::load_subagents_dir(&home_subagents).unwrap_or_default();
    let ws_md = reflect_subagent::load_subagents_dir(&ws_subagents).unwrap_or_default();
    let toml_specs = initial_cfg.subagents.clone();
    let mut configs = reflect_subagent::merge_by_priority(vec![home_md, ws_md, toml_specs]);

    // 全部源为空时回退硬编码 explorer(向后兼容)。
    if configs.is_empty() {
        configs.push(reflect_config::SubagentSpecConfig {
            name: "Explorer".into(),
            role: "explorer".into(),
            model: None,
            system_prompt:
                "You are an explorer subagent. Inspect the codebase and return a concise summary."
                    .into(),
            allowed_tools: vec!["bash".into(), "read".into(), "grep".into(), "glob".into()],
            allowed_skills: vec![],
            max_turns: None,
        });
    }

    // v1.4 子代理可见性:`call_<role>` 是运行时动态注册的工具,不在静态
    // `ALWAYS_ON_TOOLS` 中;而 `pre_loop` 按 `m4.skills.active_tool_names()`
    // 计算 `effective_tools`(模型可见工具集)。不在此把 `call_<role>` 补入
    // always-on 可见集,模型请求里就没有子代理工具的 schema,LLM 永远无法
    // 委派(离线 mock provider 无视实际工具列表回放脚本,只能靠运行时
    // 真实 LLM 测试暴露)。
    let call_tool_names: Vec<String> =
        configs.iter().map(|sc| format!("call_{}", sc.role)).collect();

    // 把合并后的 `SubagentSpecConfig` 映射为 `SubAgentSpec` 并注册为
    // `call_<role>` 工具。`data_transfer` 走默认配置。
    for sc in configs {
        let spec = reflect_subagent::SubAgentSpec {
            name: sc.name,
            role: sc.role.clone(),
            model: sc.model,
            system_prompt: sc.system_prompt,
            allowed_tools: sc.allowed_tools,
            data_transfer: Default::default(),
            max_turns: sc.max_turns,
            allowed_skills: sc.allowed_skills,
        };
        let role = sc.role;
        let tool: Arc<dyn reflect_tools::Tool> =
            Arc::new(CallSubAgentTool::new(factory.clone(), spec));
        // v1.3:走 `register_runtime_tool` 应用 Runtime 源安全 floor。
        tools().register_runtime_tool(tool);
        tracing::debug!(role = %role, "registered subagent spec");
    }

    // 补入 always-on 可见集(见上方 v1.4 注释)。
    m4.skills.add_always_on_tools(call_tool_names);

    let _ = (workspace, agent_name);
    factory
}

// ── M6 引导 (v0.3: MCP) ────────────────────────────────────────────────

/// v0.3: 启动 v0.3 M6 MCP server 集合。
///
/// 流程:
/// 1. 校验 `[mcp_servers]` 配置,失败 → warn + 整段跳过(单 server 错不阻塞)。
/// 2. 构造 `McpConnectionManager` + 内置 mpsc channel `internal_tx` →
///    `McpLifecycleEvent`,由后台 task 转成 `EventMsg::McpServerStarted/Failed`
///    推到 `event_tx` 经 JSONL stdout drainer 输出(M8 沿用的模式)。
/// 3. 对每个 server 并发 `tokio::spawn` 启动任务 → `start_server` → 拿到
///    `McpServerHandle` 后逐个 `register_if_absent` 把 adapter 写到
///    `ToolRegistry::Runtime`。
/// 4. 返回 `Arc<McpConnectionManager>`,给 reload task 用于 diff 重启。
///
/// 单 server 启动失败(spawn / initialize / list_tools)仅 warn,不阻塞
/// 其它 server 与 agent 启动。
pub(crate) async fn bootstrap_m6(
    initial_cfg: &reflect_config::ReflectConfig,
    event_tx: tokio::sync::mpsc::Sender<reflect_protocol::Event>,
) -> Option<Arc<McpConnectionManager>> {
    let configs = match initial_cfg.mcp_server_configs() {
        Ok(c) if c.is_empty() => return None,
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "MCP config invalid; skipping all MCP servers");
            return None;
        }
    };
    let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<McpLifecycleEvent>(32);
    let manager = Arc::new(McpConnectionManager::new(internal_tx));

    // 后台 task:internal event → protocol Event → event_tx (经 JSONL drainer 推到 stdout)。
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        while let Some(evt) = internal_rx.recv().await {
            let msg = match evt {
                McpLifecycleEvent::Started {
                    server,
                    tools,
                    tool_names,
                    transport,
                } => reflect_protocol::EventMsg::McpServerStarted(
                    reflect_protocol::McpServerStartedEvent {
                        server,
                        tool_count: tools,
                        tool_names,
                        transport: transport_mirror_from_runtime(transport),
                    },
                ),
                McpLifecycleEvent::Failed {
                    server,
                    error,
                    will_retry,
                } => reflect_protocol::EventMsg::McpServerFailed(
                    reflect_protocol::McpServerFailedEvent {
                        server,
                        error,
                        will_retry,
                    },
                ),
                McpLifecycleEvent::Stopped { server: _ } => {
                    // v0.3 不发 protocol event;reload task 已经把反注册事件喂回 caller。
                    continue;
                }
            };
            if event_tx_clone
                .send(reflect_protocol::Event::new(
                    reflect_protocol::EVENT_ID_NONE,
                    msg,
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 并发启动每个 server —— stdio spawn + initialize 网络 I/O 受限于
    // server 数通常 <10,tokio::spawn 足够轻量;每个 task 拿到 manager
    // 句柄直接注册 tool 到共享 ToolRegistry。
    let tools = tools();
    for cfg_shape in &configs {
        let cfg: McpServerConfig = McpServerConfig::from(cfg_shape.clone());
        let manager_clone = manager.clone();
        let tools_clone = tools.clone();
        // adapter 调用完成后 emit `McpToolInvoked`,与生命周期事件走同一
        // JSONL drainer 到 stdout(TUI / headless 消费者按 call_id 配对)。
        let invoked_tx = event_tx.clone();
        tokio::spawn(async move {
            match manager_clone.start_server(cfg.clone()).await {
                Ok(handle) => {
                    for desc in &handle.tools {
                        let adapter = McpToolAdapter::from_descriptor(
                            handle.inner.clone(),
                            desc,
                            &cfg.name,
                            cfg.timeout,
                            Some(invoked_tx.clone()),
                        );
                        let arc: Arc<dyn reflect_tools::Tool> = Arc::new(adapter);
                        // v1.3:MCP 工具改走 `ToolSource::Mcp` + 安全 floor。
                        if !tools_clone
                            .register_if_absent_with_floor(reflect_tools::ToolSource::Mcp, arc)
                        {
                            tracing::warn!(
                                tool = %desc.full_name,
                                "MCP tool name collision, skipped"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %cfg.name,
                        error = %e,
                        "MCP server failed to start"
                    );
                }
            }
        });
    }
    tracing::info!(
        mcp_servers = configs.len(),
        "MCP bootstrap: spawning start tasks"
    );
    Some(manager)
}

/// `reflect_mcp::McpTransport` → `reflect_protocol::McpTransportMirror`。
pub(crate) fn transport_mirror_from_runtime(
    t: reflect_mcp::McpTransport,
) -> reflect_protocol::McpTransportMirror {
    match t {
        reflect_mcp::McpTransport::Stdio => reflect_protocol::McpTransportMirror::Stdio,
        reflect_mcp::McpTransport::Http => reflect_protocol::McpTransportMirror::Http,
        reflect_mcp::McpTransport::Sse => reflect_protocol::McpTransportMirror::Sse,
    }
}

/// v0.5: 启动 LSP server 集合并注册 `lsp` 工具。
///
/// 流程(完全镜像 `bootstrap_m6`):
/// 1. 校验 `[lsp_servers]` 配置,失败 → warn + 整段跳过。
/// 2. 构造 `LspConnectionManager` + 内置 mpsc channel `internal_tx` →
///    `LspLifecycleEvent`,由后台 task 转成
///    `EventMsg::LspServerStarted/Failed` 推到 `event_tx`。
/// 3. 单例注册 `LspTool` 到 `ToolRegistry::Runtime`(`lsp` 走 single tool
///    + `action` enum 分派,不需要 per-method 注册)。
/// 4. 返回 `Arc<LspConnectionManager>`,给 reload task 用于 diff 重启(Phase B)。
///
/// 单 server 启动失败仅 warn,不阻塞其它 server 与 agent 启动。
pub(crate) async fn bootstrap_lsp(
    initial_cfg: &reflect_config::ReflectConfig,
    event_tx: tokio::sync::mpsc::Sender<reflect_protocol::Event>,
    tools: Arc<ToolRegistry>,
) -> Option<Arc<LspConnectionManager>> {
    let configs = match initial_cfg.lsp_server_configs() {
        Ok(c) if c.is_empty() => return None,
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "LSP config invalid; skipping all LSP servers");
            return None;
        }
    };
    let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<LspLifecycleEvent>(32);
    let manager = Arc::new(LspConnectionManager::new(internal_tx));

    // 后台 task:internal event → protocol Event → event_tx。
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        while let Some(evt) = internal_rx.recv().await {
            let msg = match evt {
                LspLifecycleEvent::Started {
                    server,
                    methods,
                    language_ids,
                } => reflect_protocol::EventMsg::LspServerStarted(
                    reflect_protocol::LspServerStartedEvent {
                        server,
                        methods,
                        language_ids,
                    },
                ),
                LspLifecycleEvent::Failed {
                    server,
                    error,
                    will_retry,
                } => reflect_protocol::EventMsg::LspServerFailed(
                    reflect_protocol::LspServerFailedEvent {
                        server,
                        error,
                        will_retry,
                    },
                ),
                LspLifecycleEvent::Stopped { server: _ } => continue,
            };
            if event_tx_clone
                .send(reflect_protocol::Event::new(
                    reflect_protocol::EVENT_ID_NONE,
                    msg,
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 单例 LspTool 注册(对照 exec/lib.rs:794 的 register_if_absent 调用)。
    let tool = Arc::new(LspTool::new(manager.clone()));
    // v1.3:走 `register_runtime_tool` 应用 Runtime 源安全 floor。
    tools.register_runtime_tool(tool);

    // 并发启动每个 server。
    for cfg_shape in &configs {
        let cfg: reflect_lsp::LspServerConfig = match cfg_shape.clone().try_into() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "LSP config shape→strong-type conversion failed");
                continue;
            }
        };
        let mgr = manager.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr.start_server(cfg).await {
                tracing::warn!(error = %e, "LSP server failed to start");
            }
        });
    }
    tracing::info!(
        lsp_servers = configs.len(),
        "LSP bootstrap: spawning start tasks"
    );
    Some(manager)
}

/// 读取先前 session 的 JSONL,重建适用于 `AgentThread` 继续运行的初始
/// 消息列表(工具调用对忠实重建,语义见 `reflect_core::resume`)。
/// 返回(可能为空的)消息列表与匹配的 `ThreadId`。
pub(crate) async fn bootstrap_resume(thread_id_str: &str) -> anyhow::Result<ResumeBundle> {
    let parsed = uuid::Uuid::parse_str(thread_id_str)
        .map_err(|e| anyhow::anyhow!("invalid thread id '{thread_id_str}': {e}"))?;
    let tid = ThreadId(parsed);
    let base = reflect_rollout::path::default_base();
    let records = reflect_rollout::reader::replay(&base, tid).await?;

    // v1.x:记录 → ChatMessage 的映射抽到 `reflect_core::resume`
    // (GUI 等非 exec 宿主复用同一语义;exec 只保留 uuid 解析 + replay 外壳)。
    let messages = reflect_core::resume::records_to_preload(&records);
    Ok(ResumeBundle {
        thread_id: tid,
        initial_messages: messages,
    })
}

/// [`bootstrap_resume`] 的结果:匹配的 `ThreadId` 与重建出的初始消息。
pub struct ResumeBundle {
    pub thread_id: ThreadId,
    pub initial_messages: Vec<reflect_llm::ChatMessage>,
}

/// 无可用 LLM 客户端时使用的 fallback summarizer。始终报错,让
/// compactor 退回 smart_prune。
pub(crate) struct NoopSummarizer;

#[async_trait::async_trait]
impl Summarizer for NoopSummarizer {
    async fn summarize_full(&self, _: &[ChatMessage]) -> Result<String, SummarizerError> {
        Err(SummarizerError::Cancelled)
    }
    async fn summarize_recent(
        &self,
        _: &[ChatMessage],
        _: Option<&str>,
    ) -> Result<String, SummarizerError> {
        Err(SummarizerError::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{ContentBlock, MessageRole, RolloutRecord, ThreadId, TurnId};

    /// 构造一个临时 HOME,在其下 `.reflect/sessions/<今天>/<tid>.jsonl` 写入
    /// 给定 records,返回 tid 字符串。`bootstrap_resume` 通过 `default_base()`
    /// 读 `$HOME/.reflect/sessions`,因此临时 HOME 能隔离测试。
    fn write_session(records: &[RolloutRecord]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        // 找出 SessionMeta 拿 tid;若无则造一个。
        let tid = records
            .iter()
            .find_map(|r| match r {
                RolloutRecord::SessionMeta { session_id, .. } => Some(*session_id),
                _ => None,
            })
            .unwrap_or_else(ThreadId::new);
        let base = dir.path().join(".reflect").join("sessions");
        let path = reflect_rollout::path::session_path_at(&base, tid, chrono::Utc::now());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        for r in records {
            writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
        }
        (dir, tid.0.to_string())
    }

    /// v1.2 P2:旧格式(Value::String)向后兼容 —— resume 仍能还原纯文本消息。
    #[tokio::test]
    async fn resume_legacy_string_format() {
        let tid = ThreadId::new();
        let records = vec![
            RolloutRecord::SessionMeta {
                session_id: tid,
                model: "m".into(),
                started_at: chrono::Utc::now(),
                workspace: None,
            },
            RolloutRecord::message(
                TurnId::new(),
                MessageRole::User,
                serde_json::Value::String("hi".into()),
            ),
            RolloutRecord::message(
                TurnId::new(),
                MessageRole::Assistant,
                serde_json::Value::String("hello".into()),
            ),
        ];
        let (_guard, id) = write_session(&records);
        let home = _guard.path().to_path_buf();
        with_isolated_home(&home, || async move {
            let bundle = bootstrap_resume(&id).await.unwrap();
            assert_eq!(bundle.initial_messages.len(), 2);
            // User → Text block
            match &bundle.initial_messages[0] {
                reflect_llm::ChatMessage::User(uc) => {
                    assert_eq!(uc.blocks.len(), 1);
                    match &uc.blocks[0] {
                        reflect_llm::ContentBlock::Text { text } => assert_eq!(text, "hi"),
                        other => panic!("expected Text block, got {other:?}"),
                    }
                }
                other => panic!("expected User, got {other:?}"),
            }
            // Assistant → text field
            match &bundle.initial_messages[1] {
                reflect_llm::ChatMessage::Assistant(ac) => {
                    assert_eq!(ac.text.as_deref(), Some("hello"));
                }
                other => panic!("expected Assistant, got {other:?}"),
            }
        })
        .await;
    }

    /// v1.2 P2:新格式(Value::Array ContentBlocks)—— user 消息完整还原。
    #[tokio::test]
    async fn resume_new_format_user_blocks() {
        let tid = ThreadId::new();
        let user_blocks = vec![ContentBlock::Text {
            text: "what is 2+2?".into(),
        }];
        let records = vec![
            RolloutRecord::SessionMeta {
                session_id: tid,
                model: "m".into(),
                started_at: chrono::Utc::now(),
                workspace: None,
            },
            RolloutRecord::message(
                TurnId::new(),
                MessageRole::User,
                serde_json::to_value(&user_blocks).unwrap(),
            ),
        ];
        let (_guard, id) = write_session(&records);
        let home = _guard.path().to_path_buf();
        with_isolated_home(&home, || async move {
            let bundle = bootstrap_resume(&id).await.unwrap();
            assert_eq!(bundle.initial_messages.len(), 1);
            match &bundle.initial_messages[0] {
                reflect_llm::ChatMessage::User(uc) => {
                    assert_eq!(uc.blocks.len(), 1);
                    match &uc.blocks[0] {
                        reflect_llm::ContentBlock::Text { text } => {
                            assert_eq!(text, "what is 2+2?")
                        }
                        other => panic!("expected Text block, got {other:?}"),
                    }
                }
                other => panic!("expected User, got {other:?}"),
            }
        })
        .await;
    }

    /// v1.2 P2:新格式 assistant —— Text + ToolUse + ToolResult 完整还原,
    /// 且 ToolResult 拆成独立 ChatMessage::Tool(provider 要求交替)。
    #[tokio::test]
    async fn resume_new_format_assistant_with_tool_calls() {
        let tid = ThreadId::new();
        let assistant_blocks = vec![
            ContentBlock::Text {
                text: "let me check".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                args: serde_json::json!({"path": "/tmp/x"}),
            },
            ContentBlock::ToolResult {
                call_id: "call_1".into(),
                output: reflect_protocol::ToolOutput {
                    content: vec![ContentBlock::Text {
                        text: "file contents".into(),
                    }],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                },
            },
        ];
        let records = vec![
            RolloutRecord::SessionMeta {
                session_id: tid,
                model: "m".into(),
                started_at: chrono::Utc::now(),
                workspace: None,
            },
            RolloutRecord::message(
                TurnId::new(),
                MessageRole::Assistant,
                serde_json::to_value(&assistant_blocks).unwrap(),
            ),
        ];
        let (_guard, id) = write_session(&records);
        let home = _guard.path().to_path_buf();
        with_isolated_home(&home, || async move {
            let bundle = bootstrap_resume(&id).await.unwrap();
            // ToolResult 拆出独立 Tool 消息,所以是 2 条:Assistant(含 tool_calls) + Tool
            assert_eq!(
                bundle.initial_messages.len(),
                2,
                "got {:?}",
                bundle.initial_messages
            );
            // 第一条:Assistant,含 text + tool_calls
            match &bundle.initial_messages[0] {
                reflect_llm::ChatMessage::Assistant(ac) => {
                    assert_eq!(ac.text.as_deref(), Some("let me check"));
                    assert_eq!(ac.tool_calls.len(), 1);
                    assert_eq!(ac.tool_calls[0].id, "call_1");
                    assert_eq!(ac.tool_calls[0].name, "read_file");
                }
                other => panic!("expected Assistant, got {other:?}"),
            }
            // 第二条:Tool(ToolResult),call_id 配对
            match &bundle.initial_messages[1] {
                reflect_llm::ChatMessage::Tool(tr) => {
                    assert_eq!(tr.call_id, "call_1");
                    assert!(!tr.is_error);
                }
                other => panic!("expected Tool, got {other:?}"),
            }
        })
        .await;
    }

    /// resume 测试改写全局 `HOME` 隔离 session 目录,并行执行会互相踩
    /// (`bootstrap_resume` 读到别的测试的临时 HOME → 0 条消息)。
    /// 互斥锁把这三个测试串行化;结束时还原原 HOME。用 tokio Mutex:
    /// 锁要横跨测试体的 await 点,std Mutex 会触发
    /// `clippy::await_holding_lock`。
    static HOME_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn with_isolated_home<F, Fut>(home: &std::path::Path, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = HOME_MUTEX.lock().await;
        let orig = std::env::var("HOME").ok();
        // SAFETY: 已持锁串行化;测试 runtime 为 current_thread,
        // 无其他线程在本测试改写 HOME 期间读取。
        unsafe {
            std::env::set_var("HOME", home);
        }
        f().await;
        // SAFETY: 同上,仍持锁。
        unsafe {
            match orig {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
