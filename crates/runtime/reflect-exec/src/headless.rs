//! headless 会话 bootstrap —— `reflect exec` 与 `reflect serve` 的共享装配层。
//!
//! v1.3 SDK:原 `async_main` 的装配段(config / hooks / registry / 工具 /
//! task / MCP / LSP / plugin / reload)抽取为三个可复用函数,exec 与
//! serve 共用,保证两条入口的行为不再漂移:
//!
//! - [`bootstrap_common`]:config 加载 + tracing + 工具注册表 + 取消令牌
//!   + workspace 解析(两个分支的前半段);
//! - [`bootstrap_normal`]:普通分支后半段(M4/M5 bootstrap、AgentThread、
//!   cron、hooks、MCP/LSP/plugin、reload drainer);
//! - [`bootstrap_resumed`]:resume 分支(回放历史 + 最小依赖)。

use std::path::PathBuf;
use std::sync::Arc;

use reflect_config::ConfigWatcher;
use reflect_core::{AgentConfig, AgentThread};
use reflect_hooks::builtins::{PlanModeGate, build_read_before_edit};
use reflect_llm::ModelRegistry;
use reflect_mcp::McpConnectionManager;
use reflect_protocol::{Event, ThreadId};
use reflect_rollout::JsonlRolloutWriter;
use reflect_task::coordinator::CoordinatorConfig;
use reflect_tools::{Sanitizer, ToolRegistry, ToolSource};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bootstrap::{
    self, bootstrap_lsp, bootstrap_m4, bootstrap_m5, bootstrap_m6, bootstrap_resume,
};
use crate::bootstrap_plugins;
use crate::jsonl::JsonlWriter;
use crate::reload::spawn_reload_task;
use crate::runtime_config::{
    apply_coordinator_from_config, build_quota_tracker, build_sanitizer, build_telemetry_sink,
    init_tracing,
};

/// headless bootstrap 的公共参数(exec / serve 各自的 args 投影)。
#[derive(Debug, Clone, Default)]
pub struct HeadlessArgs {
    /// Agent 定义名(默认 `"default"`)。
    pub agent: Option<String>,
    /// 任务 / 团队用内存 store(不落盘)。
    pub ephemeral_tasks: bool,
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根作为工作区。
    pub auto_root: bool,
    /// 启动即进入 Plan mode(只读)。
    pub plan_mode: bool,
    /// 内置 hook 仅在 `[hooks].enabled` 显式列出时启用(serve 用)。
    /// exec / TUI 保持「未配置 = 全部启用」;serve 是 SDK 嵌入入口,
    /// 默认启用 `verification` 之类 Stop hook 会在宿主 cwd 跑
    /// `cargo test`,对宿主进程既慢又不可预期。
    pub hooks_explicit_only: bool,
}

/// [`bootstrap_common`] 的产物:两个分支后续装配都要用的共享句柄。
pub struct HeadlessCommon {
    pub initial_cfg: reflect_config::ReflectConfig,
    pub config_path: PathBuf,
    pub registry: Arc<ModelRegistry>,
    pub hook_engine: Arc<reflect_hooks::HookEngine>,
    pub sanitizer: Arc<Sanitizer>,
    pub tools: Arc<ToolRegistry>,
    pub task_manager: Arc<reflect_task::TaskManager>,
    pub cancel: CancellationToken,
    pub workspace: PathBuf,
    /// `"provider/model"` 形式的完整 spec。
    pub model: String,
    pub watcher: ConfigWatcher,
    /// reload / MCP / LSP 生命周期事件的公共通道(消费者由各分支自定)。
    pub reload_tx: mpsc::Sender<Event>,
    /// `reload_tx` 的接收端(normal 分支桥接到 stdout drainer;resumed
    /// 分支 drop 掉,与 exec 历史行为一致)。
    pub(crate) _reload_rx: ReloadRxHandle,
}

/// [`bootstrap_normal`] / [`bootstrap_resumed`] 的产物。
pub struct HeadlessSession {
    pub thread: Arc<AgentThread>,
    pub tools: Arc<ToolRegistry>,
    /// resume 分支回放进 thread 的历史消息数(普通分支为 0)。exec 用它
    /// 拼合成 reminder 文案;serve 忽略。
    pub prior_messages: usize,
}

/// 装配公共前半段:config → tracing → 工具注册表 → task manager →
/// 取消令牌 → workspace。与原 `async_main` 步骤 0-3 逐行对应。
pub async fn bootstrap_common(args: &HeadlessArgs) -> anyhow::Result<HeadlessCommon> {
    // 0. 尽早加载配置,让 `init_tracing` 能按 [analytics] 决定是否启用 OTLP。
    let config_path = reflect_config::default_config_path()
        .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot locate config directory"))?;
    let initial_cfg = reflect_config::load_default();
    init_tracing(initial_cfg.analytics.as_ref());

    // 把 `[sandbox].os_level` 配置桥接到 env(`REFLECT_SANDBOX_OS_LEVEL`)
    // —— `BashTool` 在 execute 时读 env 决定是否激活 Seatbelt/Landlock。
    // env 优先于 TOML(让 `REFLECT_SANDBOX_OS_LEVEL=0` 能临时覆盖配置)。
    if initial_cfg.sandbox.os_level && std::env::var_os("REFLECT_SANDBOX_OS_LEVEL").is_none() {
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "1");
        }
        tracing::info!(
            "[sandbox].os_level = true → REFLECT_SANDBOX_OS_LEVEL=1 (bash 将在 OS 沙箱内执行)"
        );
    }

    // `[sanitize]` 段接到 queue 的脱敏 pass;失败 → warn + fallback 默认。
    let sanitizer = build_sanitizer(initial_cfg.sanitize.as_ref());

    // 从 `~/.reflect/config.toml [hooks]` 段构建 HookEngine(含 builtin +
    // 插件 hook),AgentThread 与 TaskManager 共享同一份。
    // serve(hooks_explicit_only)把「未配置 = 全部启用」收紧为「未配置
    // = 不启用任何内置 hook」:嵌入场景不应默认跑 verification 之类
    // 会在宿主 cwd 执行 shell 命令的 Stop hook;显式列出的照常启用。
    let mut hooks_cfg =
        reflect_hooks::config::HooksConfig::from_reflect_section(&initial_cfg.hooks);
    if args.hooks_explicit_only && hooks_cfg.enabled.is_none() {
        hooks_cfg.enabled = Some(Vec::new());
    }
    let hook_engine: Arc<reflect_hooks::HookEngine> = Arc::new(hooks_cfg.build_engine());

    // 1. ModelRegistry(env 兜底;v1.3 起 `REFLECT_MODEL=mock` 可离线注册
    //    mock provider)+ 热重载 watcher。
    let registry = Arc::new(ModelRegistry::new());
    initial_cfg
        .apply_to_registry(&registry)
        .map_err(|e| anyhow::anyhow!("failed to build provider from config: {e}"))?;
    // 无 provider / 无显式 model 都要 fail-fast 且报因可读 —— 不再回落
    // 内置默认模型名(打向第三方兼容端点只会得到难诊断的远端错误)。
    if initial_cfg.active_provider().is_none() {
        anyhow::bail!(
            "no provider configured: set [active] in {} or OPENAI_API_KEY/ANTHROPIC_API_KEY env, or REFLECT_MODEL=mock",
            config_path.display()
        );
    }
    let model = initial_cfg.resolved_model_spec().ok_or_else(|| {
        anyhow::anyhow!(
            "no model configured for the active provider: set [<provider>].model \
             or a [[<provider>.credentials]].model entry in {} (or REFLECT_MODEL env)",
            config_path.display()
        )
    })?;

    let watcher = ConfigWatcher::spawn(config_path.clone(), initial_cfg.clone()).map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn config watcher at {}: {e}",
            config_path.display()
        )
    })?;
    // `ConfigReloaded` 事件的专用 channel;消费者由各分支接。
    let (reload_tx, reload_rx) = mpsc::channel::<Event>(8);
    // 把 reload_rx 存进 HeadlessCommon 之外单独带回 —— 直接作为字段携带
    // Receiver 会让 HeadlessCommon 无法 Clone;这里包成消费端句柄。
    let reload_rx = ReloadRxHandle(reload_rx);

    // 2. 工具注册表(builtin 全量注册,与 exec 历史行为一致)。
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(reflect_tools::builtins::EchoTool));
    tools.register(Arc::new(reflect_tools::builtins::BashTool));
    // 核心文件工具:与 lib facade(`reflect::builder::default_tool_registry`)对齐。
    // 此前缺失导致 CLI headless/serve 下模型调用 read/write/edit/grep/glob 报
    // "tool not found",而 ALWAYS_ON_TOOLS prompt 仍向模型宣告这些工具可用。
    tools.register(Arc::new(reflect_tools::builtins::ReadTool));
    tools.register(Arc::new(reflect_tools::builtins::WriteTool));
    tools.register(Arc::new(reflect_tools::builtins::EditTool));
    tools.register(Arc::new(reflect_tools::builtins::DeleteTool));
    tools.register(Arc::new(reflect_tools::builtins::GrepTool));
    tools.register(Arc::new(reflect_tools::builtins::GlobTool));
    tools.register(Arc::new(reflect_tools::builtins::EnterPlanModeTool));
    tools.register(Arc::new(reflect_tools::builtins::ExitPlanModeTool));
    tools.register(Arc::new(reflect_tools::builtins::PlanWriteTool));
    tools.register(Arc::new(reflect_tools::builtins::EnterWorktreeTool));
    tools.register(Arc::new(reflect_tools::builtins::ExitWorktreeTool));
    tools.register(Arc::new(reflect_ast::AstTool::new()));
    tools.register(Arc::new(reflect_tools::builtins::WebFetchTool::new()));
    tools.register(Arc::new(reflect_tools::builtins::WebSearchTool::new()));
    let ask_uq_limits = initial_cfg
        .ask_user_question
        .as_ref()
        .and_then(|s| s.resolved())
        .unwrap_or(reflect_config::schema::ResolvedAskUserQuestion {
            max_questions: 4,
            max_options: 4,
            default_timeout_secs: 900,
        });
    tools.register(Arc::new(reflect_tools::builtins::AskUserQuestionTool {
        max_questions: ask_uq_limits.max_questions,
    }));
    tools.register(Arc::new(reflect_tools::builtins::AskUserTool));
    tools.register(Arc::new(reflect_tools::builtins::RequestHumanInputTool));
    tools.register(Arc::new(reflect_tools::builtins::ToolSearchTool::new(
        tools.clone(),
    )));
    tools.register(Arc::new(reflect_tools::builtins::ImageViewTool));
    tools.register(Arc::new(reflect_tools::builtins::NotebookEditTool));
    tools.register(Arc::new(reflect_tools::builtins::BriefTool));
    tools.register(Arc::new(reflect_tools::builtins::GetContextRemainingTool));
    tools.register(Arc::new(crate::CheckpointTool));
    tools.register(Arc::new(crate::RewindTool));

    // task 系统:默认 FileTaskStore 持久化,`--ephemeral-*` / HOME 缺失
    // 时降级内存后端。
    let task_store: Arc<dyn reflect_task::TaskStore> = if args.ephemeral_tasks {
        Arc::new(reflect_task::InMemoryTaskStore::default())
    } else {
        match reflect_task::FileTaskStore::with_default_home() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "FileTaskStore 初始化失败,降级到 InMemoryTaskStore"
                );
                Arc::new(reflect_task::InMemoryTaskStore::default())
            }
        }
    };
    let team_store: Arc<dyn reflect_task::TeamStore> = if args.ephemeral_teams {
        Arc::new(reflect_task::InMemoryTeamStore::default())
    } else {
        match reflect_task::FileTeamStore::with_default_home() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "FileTeamStore 初始化失败,降级到 InMemoryTeamStore"
                );
                Arc::new(reflect_task::InMemoryTeamStore::default())
            }
        }
    };
    let task_manager = Arc::new(
        reflect_task::TaskManager::new(task_store, team_store)
            .with_hook_engine(hook_engine.clone()),
    );
    reflect_task::register_task_tools(&tools, task_manager.clone());
    bootstrap::TOOLS.with(|t| *t.borrow_mut() = Some(tools.clone()));

    // v1.4 D1:tokenizer feature 开启时注册 tiktoken 全局估算器
    // (set-once;失败仅 warn,回退启发式,不阻塞启动)。
    #[cfg(feature = "tokenizer")]
    match reflect_compact::global_tiktoken_estimator() {
        Ok(est) => {
            if reflect_compact::set_global_estimator(est) {
                tracing::info!("tiktoken estimator registered (tokenizer feature)");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "tiktoken estimator init failed; using heuristic");
        }
    }

    // 3. Ctrl-C 取消令牌 + workspace 解析。
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace = if args.auto_root {
        reflect_core::detect_project_root(&workspace)
    } else {
        workspace
    };

    Ok(HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        tools,
        task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: reload_rx,
    })
}

/// `HeadlessCommon` 内部携带的 reload 接收端句柄。
///
/// `bootstrap_normal` / `bootstrap_resumed` 消费它(桥接到 stdout);
/// `HeadlessCommon` 被 drop 时接收端一并释放,reload task 的发送端
/// send 失败后自然退出,不会泄漏。
pub(crate) struct ReloadRxHandle(pub(crate) mpsc::Receiver<Event>);

/// 装配普通分支后半段(与原 `async_main` 普通分支逐行对应),返回
/// 可供 exec / serve 使用的 session。
pub async fn bootstrap_normal(
    common: HeadlessCommon,
    args: &HeadlessArgs,
) -> anyhow::Result<HeadlessSession> {
    let HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        tools,
        task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: ReloadRxHandle(reload_rx),
    } = common;

    // M4 bootstrap:agent 定义、skills、压缩器、记忆、prompt 构建器。
    let thread_id = ThreadId::new();
    let telemetry_sink = build_telemetry_sink(&initial_cfg);
    let m4 = bootstrap_m4(
        &workspace,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        initial_cfg.compact.trigger_tokens,
        &initial_cfg,
        telemetry_sink.clone(),
    )
    .expect("bootstrap_m4 must succeed for the normal (non-resume) path");
    let policy = Arc::new(initial_cfg.routing_policy());

    // M5 bootstrap:subagent 工具(call_<role>)。
    let parent_recorder: Arc<dyn reflect_protocol::RolloutRecorder> = m4
        .recorder
        .clone()
        .unwrap_or_else(|| Arc::new(reflect_protocol::NullRecorder));
    let factory = bootstrap_m5(
        &workspace,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        parent_recorder.clone(),
        &m4,
        &initial_cfg,
    );
    factory.set_telemetry(telemetry_sink.clone());
    // v1.4 A1:子代理运行注册表 + 主会话令牌注入 —— 同一 `Arc` 双侧共享
    // (主 cfg 的 `Op::Interrupt { child_id }` 路由侧 / 工厂的 spawn 登记
    // 侧);同时把主会话 Ctrl-C 令牌覆盖进 factory,让子代理的 child_token
    // 挂在会话令牌之下,Shutdown / Ctrl-C 能级联取消在飞子代理。
    let subagent_runtime = Arc::new(reflect_core::SubagentRuntimeRegistry::new());
    factory.set_runtime_registry(subagent_runtime.clone());
    factory.set_cancel(cancel.clone());

    // Coordinator 模式整合:team spec 注入 factory + 统一开关。
    let coord_enabled = CoordinatorConfig::from_env_or_config(
        initial_cfg
            .coordinator
            .as_ref()
            .unwrap_or(&reflect_config::CoordinatorSection::default()),
    )
    .enabled;
    if coord_enabled {
        if let Err(e) = task_manager.sync_team_specs(&factory).await {
            tracing::warn!(error = %e, "coordinator: sync_team_specs 失败");
        }
    }
    apply_coordinator_from_config(&initial_cfg, &workspace, Some(&m4), &factory, &tools);

    let skills_for_plugins = m4.skills.clone();
    let token_budget = reflect_core::config::token_budget_from_env(
        initial_cfg
            .token_budget
            .as_ref()
            .and_then(|s| s.session_total_tokens),
    );
    let max_iterations =
        reflect_core::config::max_iterations_from_env(initial_cfg.active.max_iterations);
    let quota_tracker = build_quota_tracker(&initial_cfg);
    // web_search 工具级 env:`[web_search].api_key` → BRAVE_API_KEY。
    let mut tool_env = std::collections::HashMap::new();
    if let Some(ws) = &initial_cfg.web_search {
        if let Some(key) = &ws.api_key {
            if !key.is_empty() {
                tool_env.insert("BRAVE_API_KEY".to_string(), key.clone());
            }
        }
    }
    let mut cfg = AgentConfig::new(model.clone(), workspace)
        .with_m4(m4)
        .with_policy(policy)
        .with_token_budget(token_budget)
        .with_max_iterations(max_iterations)
        .with_quota_tracker(quota_tracker)
        .with_tool_env(tool_env)
        .with_telemetry(telemetry_sink.clone())
        .with_cancel(cancel.clone())
        .with_subagent_runtime(subagent_runtime.clone())
        .with_context_window_overrides(
            initial_cfg
                .context_windows
                .as_ref()
                .map(|c| c.entries.clone())
                .unwrap_or_default(),
        )
        .with_initial_permission_mode(if args.plan_mode {
            reflect_protocol::PermissionMode::Plan
        } else {
            reflect_protocol::PermissionMode::Auto
        });
    if let Ok(file_store) = reflect_permissions::FilePermissionStore::with_default_home() {
        let file_store: Arc<dyn reflect_permissions::PermissionStore> = Arc::new(file_store);
        let cfg_store: Arc<dyn reflect_permissions::PermissionStore> =
            Arc::new(reflect_permissions::InMemoryPermissionStore::new());
        if let Some(sec) = &initial_cfg.permissions {
            let rules = sec.expanded_rules();
            for rule in &rules {
                if let Err(e) = cfg_store.add(rule.clone()).await {
                    tracing::warn!(error = %e, "permission rule from config.toml 落库失败,跳过");
                }
            }
            tracing::info!(
                rules = rules.len(),
                "loaded permissions rules from config.toml [permissions]"
            );
        }
        let chained: Arc<dyn reflect_permissions::PermissionStore> =
            Arc::new(reflect_permissions::ChainedPermissionStore::new(vec![
                cfg_store, file_store,
            ]));
        let resolver: Arc<dyn reflect_permissions::PermissionResolver> =
            Arc::new(reflect_permissions::StorePermissionResolver::new(chained));
        cfg = cfg.with_permission_resolver(resolver);
    }
    cfg.yolo_classifier = Some(Arc::new(reflect_permissions::HeuristicYoloClassifier));
    let cfg_for_reload = cfg.clone(); // 共享 model Arc<RwLock>
    let thread = Arc::new(AgentThread::new(
        cfg,
        registry.clone(),
        tools.clone(),
        Some(sanitizer.clone()),
        Some(hook_engine.clone()),
    ));

    // Cron 真实调度:到期把 job.prompt 作为 user_input 注入。
    let cron_scheduler = Arc::new(reflect_stream::CronScheduler::new(
        Some(thread.submission_sender()),
        thread_id,
    ));
    tools.register_with_source(
        ToolSource::Builtin,
        Arc::new(crate::CronTool::new(cron_scheduler.clone())),
    );
    let cron_driver = (*cron_scheduler).clone().start(30);
    tokio::spawn(async move {
        let _handle = cron_driver;
        std::future::pending::<()>().await;
    });
    tracing::info!(session_id = %thread_id, "cron scheduler started (tick=30s)");

    // Plan mode hooks:PlanModeGate + read_before_edit。
    thread.register_hook(PlanModeGate::default_mode());
    let rbe_section = initial_cfg.hooks.read_before_edit.as_ref();
    let (_rbe_state, rbe_hook) = build_read_before_edit(
        rbe_section.and_then(|s| s.enabled),
        rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
    );
    thread.register_hook(rbe_hook);
    tracing::info!(
        "registered read_before_edit hook (enabled={:?}, drift_ms={:?})",
        rbe_section.and_then(|s| s.enabled),
        rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
    );

    if args.plan_mode {
        tracing::info!("exec started with --plan-mode; only read-only tools are usable");
    }

    // MCP / LSP bootstrap(失败仅 warn,不阻塞主流程)。
    let mcp_manager = bootstrap_m6(&initial_cfg, reload_tx.clone()).await;
    let _lsp_manager = bootstrap_lsp(&initial_cfg, reload_tx.clone(), tools.clone()).await;

    // 插件 runtime —— 挂载 enabled_plugins 能力。
    let mcp_for_plugins = mcp_manager.clone().unwrap_or_else(|| {
        let (tx, _rx) = tokio::sync::mpsc::channel::<reflect_mcp::McpLifecycleEvent>(16);
        Arc::new(McpConnectionManager::new(tx))
    });
    tools.register(Arc::new(reflect_mcp::ListMcpResourcesTool::new(
        mcp_for_plugins.clone(),
    )));
    tools.register(Arc::new(reflect_mcp::ReadMcpResourceTool::new(
        mcp_for_plugins.clone(),
    )));

    let plugin_runtime = bootstrap_plugins::bootstrap_plugins(
        tools.clone(),
        thread.hook_engine(),
        mcp_for_plugins,
        skills_for_plugins,
        factory.clone(),
        &initial_cfg.plugins.enabled_plugins,
        Some(reload_tx.clone()),
    )
    .await;

    // reload task + `ConfigReloaded` drainer(转发到 stdout JSONL)。
    spawn_reload_task(
        registry.clone(),
        watcher,
        reload_tx,
        config_path.clone(),
        initial_cfg,
        cfg_for_reload,
        Some(factory),
        Some(parent_recorder),
        mcp_manager,
        tools.clone(),
        Some(plugin_runtime),
    );
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<Event>();
    std::thread::spawn(move || {
        for event in sync_rx {
            let mut w = JsonlWriter::new(std::io::stdout().lock());
            if let Err(e) = w.write_event(&event) {
                tracing::warn!(error = %e, "config-reload jsonl write failed; dropping");
                break;
            }
        }
        tracing::debug!("config-reload drainer thread exited");
    });
    tokio::spawn(async move {
        let mut rx = reload_rx;
        while let Some(event) = rx.recv().await {
            if sync_tx.send(event).is_err() {
                break;
            }
        }
    });

    Ok(HeadlessSession {
        thread,
        tools,
        prior_messages: 0,
    })
}

/// 装配 resume 分支(与原 `async_main` resume 分支逐行对应):回放历史
/// 消息到全新 AgentThread,不带 factory / plugin(回放补足先前状态)。
pub async fn bootstrap_resumed(
    common: HeadlessCommon,
    args: &HeadlessArgs,
    thread_id_str: &str,
) -> anyhow::Result<HeadlessSession> {
    let HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        tools,
        task_manager: _task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: ReloadRxHandle(reload_rx),
    } = common;

    let bundle = bootstrap_resume(thread_id_str).await?;
    // 最小 M4 依赖:不做 compaction,recorder 续写原 session。
    let resume_recorder: Arc<dyn reflect_protocol::RolloutRecorder> = Arc::new(
        JsonlRolloutWriter::new(reflect_rollout::path::default_base(), bundle.thread_id),
    );
    let mut m4 = reflect_core::config::default_m4_deps(args.agent.as_deref().unwrap_or("default"));
    m4.recorder = Some(resume_recorder.clone());
    let cfg = AgentConfig::new(model.clone(), workspace.clone())
        .with_m4(m4)
        .with_policy(Arc::new(initial_cfg.routing_policy()))
        .with_telemetry(build_telemetry_sink(&initial_cfg))
        .with_cancel(cancel.clone())
        .with_preload_messages(bundle.initial_messages.clone());
    let cfg_for_reload = cfg.clone();
    let thread = Arc::new(AgentThread::new(
        cfg,
        registry.clone(),
        tools.clone(),
        Some(sanitizer.clone()),
        Some(hook_engine.clone()),
    ));
    // resume 也跑 MCP bootstrap,方便继续使用之前挂的 MCP server。
    let mcp_manager = bootstrap_m6(&initial_cfg, reload_tx.clone()).await;
    let _lsp_manager = bootstrap_lsp(&initial_cfg, reload_tx.clone(), tools.clone()).await;
    spawn_reload_task(
        registry.clone(),
        watcher,
        reload_tx,
        config_path.clone(),
        initial_cfg,
        cfg_for_reload,
        None,
        Some(resume_recorder),
        mcp_manager,
        tools.clone(),
        None,
    );
    // resume 分支历史上没有 reload drainer(事件静默丢弃),保持原行为:
    // 这里同样不桥接 reload_rx,drop 即让 reload task 的发送端自然退出。
    drop(reload_rx);

    Ok(HeadlessSession {
        thread,
        tools,
        prior_messages: bundle.initial_messages.len(),
    })
}

/// 供 exec / serve 复用的 resume 三选一解析(`--resume` / `-c` / `-r`)。
pub fn resolve_resume_thread_id(
    resume: Option<&str>,
    continue_last: bool,
    resume_by: Option<usize>,
) -> anyhow::Result<Option<String>> {
    if let Some(s) = resume {
        return Ok(Some(s.to_string()));
    }
    if continue_last || resume_by.is_some() {
        let base = reflect_rollout::path::default_base();
        let tid = reflect_rollout::index::resolve_session_index(&base, continue_last, resume_by)?;
        return Ok(Some(tid.to_string()));
    }
    Ok(None)
}

/// 给 `bootstrap_resumed` 用的合成 prompt(与 exec 历史行为一致)。
pub fn resumed_prompt(thread_id: &str, prior_messages: usize) -> String {
    format!(
        "<system-reminder>Resumed session {thread_id} ({prior_messages} prior messages)</system-reminder>\n\nContinue."
    )
}
