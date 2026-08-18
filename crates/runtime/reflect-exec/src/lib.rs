#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! `reflect-exec` —— headless 二进制。读取 prompt,运行单个回合,
//! 以 JSONL 流形式向 stdout 输出事件。tracing 日志走 stderr。

mod bootstrap;
pub mod bootstrap_plugins;
mod checkpoint_tool;
mod cron_tool;
mod jsonl;
mod reload;
mod runtime_config;

#[cfg(test)]
mod tests;

// 重导出,保留历史公共 API 表面积。内部也通过 `crate::` 路径引用这些项。
pub use bootstrap::ResumeBundle;
pub use checkpoint_tool::{CheckpointTool, RewindTool};
pub use cron_tool::CronTool;
pub use jsonl::JsonlWriter;
pub use reload::{diff_sections_for_test, handle_reload, spawn_reload_task};

use std::sync::Arc;

use clap::Args;
use reflect_config::ConfigWatcher;
use reflect_core::{AgentConfig, AgentThread};
use reflect_hooks::builtins::{PlanModeGate, build_read_before_edit};
use reflect_llm::ModelRegistry;
use reflect_mcp::McpConnectionManager;
use reflect_protocol::Submission;
use reflect_protocol::ThreadId;
use reflect_rollout::JsonlRolloutWriter;
use reflect_task::coordinator::CoordinatorConfig;
use reflect_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use bootstrap::{bootstrap_lsp, bootstrap_m4, bootstrap_m5, bootstrap_m6, bootstrap_resume};
use runtime_config::{
    apply_coordinator_from_config, build_quota_tracker, build_sanitizer, build_telemetry_sink,
    init_tracing,
};

// 把 PathBuf 引入作用域(避免在文件顶部额外 use)。
use std::path::PathBuf;

/// `exec` 子命令的参数。
///
/// resume 输入语义上"三选一"(`--resume <uuid>` / `-c` / `-r N`)。历史上用
/// field-level `conflicts_with` 在 clap 层 enforce 互斥,但 `clap_derive` 的
/// `conflicts_with` 不允许 forward-ref,在 `Args` flatten 上下文里会踩坑,故曾
/// 移除互斥改由 `async_main` 的 `if/else if` 优先级链兜底 —— 但这导致
/// `resume_flags` 集成测试里 `-c` + `-r` / `-c` + `--resume` / `-r` + `--resume`
/// 三组冲突用例长期红。
///
/// v1.x 修复:改用 `conflicts_with_all`(显式列出同组其余字段名,避免
/// forward-ref 解析问题),在 clap 层恢复三者互斥。位置参数 `prompt` 与
/// resume 输入的互斥仍由 `async_main` 的优先级链兜底
/// (`--resume` > (`-c` / `-r`) > `prompt`)。
#[derive(Debug, Args)]
pub struct ExecArgs {
    /// 待执行的用户 prompt。若同时给出 `--resume` / `-c` / `-r`(优先级更高
    /// —— 详见 `async_main` 的 resume 分支),则本字段被忽略。
    pub prompt: Option<String>,
    /// 按 thread id 恢复历史 session。把 `RolloutRecord::Message` 与
    /// `Compaction` 记录重放到一个全新的 `AgentThread`,并提交一条合成的
    /// `<system-reminder>resumed session</system-reminder>` 用户消息。
    ///
    /// v1.x 回归修复:此前该字段缺 `#[arg]`,clap 不注册 `--resume` 旗标 ——
    /// `reflect exec --resume <UUID>` 直接报 "unexpected argument" 退出。
    /// 补回 long flag,与 `-c` / `-r` 一致地暴露给 CLI,并用
    /// `conflicts_with_all` 在 clap 层 enforce 三者互斥。
    #[arg(long, value_name = "UUID", conflicts_with_all = ["continue_last", "resume_by"])]
    pub resume: Option<String>,
    /// 续最近一次 session(等价 `--resume $(reflect session ls | head -1)`)。
    #[arg(long, short = 'c', conflicts_with_all = ["resume", "resume_by"])]
    pub continue_last: bool,
    /// 按序号 resume(`reflect exec -r 3` = 续第 3 条 session,1-indexed,newest first)。
    #[arg(long, short = 'r', value_name = "N", conflicts_with_all = ["resume", "continue_last"])]
    pub resume_by: Option<usize>,
    /// Agent 定义名(在 `~/.reflect/agents/*.md` 与
    /// `{workspace}/.reflect/agents/*.md` 中查找)。默认 `"default"`。
    #[arg(long)]
    pub agent: Option<String>,
    /// v1.x:启动后立即进入 Plan mode(只读调研,write 工具被 `PlanModeGate`
    /// hook blanket-deny)。等价 `/plan` slash,但作用在 `reflect exec` 的
    /// headless 单 turn 模式:agent 只能跑 read-only 工具,exit code 122
    /// 表示计划生成失败需要重试。详见 `docs/PLAN_MODE.md`。
    #[arg(long)]
    pub plan_mode: bool,
    /// S2.5:用内存 `InMemoryTaskStore` 而**不**用 `FileTaskStore` —— 任务
    /// 不会持久化到 `~/.reflect/tasks/<list>/<id>.json`。默认 `false`
    /// (持久化,与 CLI 的 `reflect task ls` 同源)。
    ///
    /// 何时启用:单元测试 / 临时跑一次不想留痕 / `HOME` 未设。
    #[arg(long, default_value_t = false)]
    pub ephemeral_tasks: bool,
    /// S2.5:用内存 `InMemoryTeamStore` 而**不**用 `FileTeamStore` —— team
    /// 不会持久化到 `~/.reflect/teams/<name>.json`。默认 `false`
    /// (持久化,与 CLI 的 `reflect task team` 同源)。
    #[arg(long, default_value_t = false)]
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根(第一个含 `.git` / `Cargo.toml` / `.reflect/` 的
    /// 目录)作为工作区。默认关;与 `reflect tui --auto-root` 对齐。
    #[arg(long, default_value_t = false)]
    pub auto_root: bool,
}

/// 入口函数:既被独立 `reflect-exec` 二进制调用,也被顶层 CLI router 的
/// `reflect exec` 子命令调用。
pub fn run(args: ExecArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(args))
}

async fn async_main(args: ExecArgs) -> anyhow::Result<()> {
    // 0. 尽早加载配置,让 `init_tracing` 能按 [analytics] 决定是否启用 OTLP。
    let config_path = reflect_config::default_config_path()
        .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot locate config directory"))?;
    let initial_cfg = reflect_config::load_default();
    init_tracing(initial_cfg.analytics.as_ref());

    // v1.2 P0-1:把 `[sandbox].os_level` 配置桥接到 env(`REFLECT_SANDBOX_OS_LEVEL`)
    // —— `BashTool` 在 execute 时读 env 决定是否激活 Seatbelt/Landlock。
    // env 优先于 TOML(让 `REFLECT_SANDBOX_OS_LEVEL=0` 能临时覆盖配置)。
    // 仅当用户未显式设 env 时,TOML `os_level=true` 才写入 env。
    if initial_cfg.sandbox.os_level && std::env::var_os("REFLECT_SANDBOX_OS_LEVEL").is_none() {
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "1");
        }
        tracing::info!(
            "[sandbox].os_level = true → REFLECT_SANDBOX_OS_LEVEL=1 (bash 将在 OS 沙箱内执行)"
        );
    }

    // v1.0.0-rc2 review 2026-06-30 P0:把 `~/.reflect/config.toml [sanitize]`
    // 段真正接到 queue 的脱敏 pass 上 —— 历史实现硬编码
    // `Sanitizer::with_defaults()`,用户的 `enabled = false` / `marker` /
    // `extra_patterns` 全部死信。从 `SanitizeSection` 字段映射到
    // `SanitizeConfig`,失败 → warn + fallback 到默认(不阻塞启动)。
    let sanitizer = build_sanitizer(initial_cfg.sanitize.as_ref());

    // v1.x:从 config.toml `[hooks]` 段构建 HookEngine(含 builtin hook +
    // 插件 hook)。此前 `HooksConfig::build_engine` 是孤儿 —— 实现完整但
    // 从未被调用,导致 AgentThread 硬编码空 `HookEngine::new()`,`[hooks]`
    // 配置整段死信(verification / plan_completion / langfuse / test_runner
    // / search_budget 五个 builtin hook 全不生效)。构建一次,AgentThread
    // 与 TaskManager 共享同一 Arc<HookEngine>。
    let hook_engine: Arc<reflect_hooks::HookEngine> = Arc::new(
        reflect_hooks::config::HooksConfig::from_reflect_section(&initial_cfg.hooks).build_engine(),
    );

    // 1. 从 `~/.reflect/config.toml` 构建 ModelRegistry(env 兜底)
    //    + 启动热重载 watcher,TOML 变更时重新应用。
    let registry = Arc::new(ModelRegistry::new());
    initial_cfg
        .apply_to_registry(&registry)
        .map_err(|e| anyhow::anyhow!("failed to build provider from config: {e}"))?;
    let provider = initial_cfg
        .active_provider()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no provider configured: set [active] in {} or OPENAI_API_KEY/ANTHROPIC_API_KEY env, or add [ollama] section",
                config_path.display()
            )
        })?
        .to_string();
    let model = format!("{}/{}", provider, initial_cfg.model_for(&provider));

    let watcher = ConfigWatcher::spawn(config_path.clone(), initial_cfg.clone()).map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn config watcher at {}: {e}",
            config_path.display()
        )
    })?;
    // M8 P1b:`ConfigReloaded` 事件的专用 channel。reload task 往这里
    // 推送;一个小型 drainer(见下)把每个事件转发给共享 JSONL writer。
    // channel 把 watcher 与 stdout 写循环解耦,慢消费者绝不阻塞 watcher。
    // v0.2.2: spawn_reload_task 推迟到 AgentThread 与 SubAgentFactory
    // 构造之后 —— 需要传 `agent_cfg.clone()` (共享 model RwLock) 与
    // factory 句柄进 reload task,这样 handle_reload 才能调
    // `agent_cfg.set_model(...)` 与 `factory.set_default_model(...)`。
    let (reload_tx, reload_rx) = mpsc::channel::<reflect_protocol::Event>(8);

    // 2. 工具注册表(M1 注册 echo + bash 但未调用)
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(reflect_tools::builtins::EchoTool));
    tools.register(Arc::new(reflect_tools::builtins::BashTool));
    tools.register(Arc::new(reflect_tools::builtins::DeleteTool));
    // v1.x Plan mode 控制面工具 —— agent 主动进入/退出 Plan mode。
    tools.register(Arc::new(reflect_tools::builtins::EnterPlanModeTool));
    tools.register(Arc::new(reflect_tools::builtins::ExitPlanModeTool));
    // v1.x Plan mode 写盘工具 —— Plan 阶段把 plan markdown 落到
    // `<workspace>/.reflect/plan/<name>.md`,供 ExitPlanMode 读取。
    // required_permission = Auto,Plan mode 下免审批直写。
    tools.register(Arc::new(reflect_tools::builtins::PlanWriteTool));
    tools.register(Arc::new(reflect_tools::builtins::EnterWorktreeTool));
    tools.register(Arc::new(reflect_tools::builtins::ExitWorktreeTool));
    // v1.0.0-rc1: AST 结构化搜索/重写工具(tree-sitter 后端,
    // 默认 5 个 grammar:rust/typescript/python/go/javascript)。
    tools.register(Arc::new(reflect_ast::AstTool::new()));
    // v1.0.0-rc4: Web 工具。WebFetch 走 `Prompt`(headless 模式下也会
    // 触发 ApprovalGate 等待),WebSearch 默认 `Auto` 直接执行。
    tools.register(Arc::new(reflect_tools::builtins::WebFetchTool::new()));
    tools.register(Arc::new(reflect_tools::builtins::WebSearchTool::new()));
    // v1.1.0 P1 #14:LLM 主动向用户发起结构化询问(`ask_user_question`)。
    // headless 模式下 `ctx.approval = None`,tool 会返回 `ToolError::Execution`
    // 让 LLM 知道该路径不可用,而不是无限阻塞。
    // v1.x:从 config.toml [ask_user_question] 注入 max_questions(此前
    // `AskUserQuestionSection.resolved()` 是孤儿,配置完全死信)。
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
    // P2 `request-human-input`:ask_user 的「持久化」变体,把交互记录到
    // metadata(context_id)。此前结构已实现但漏注册到 registry,agent 无法调用。
    tools.register(Arc::new(reflect_tools::builtins::RequestHumanInputTool));
    tools.register(Arc::new(reflect_tools::builtins::ToolSearchTool::new(
        tools.clone(),
    )));
    tools.register(Arc::new(reflect_tools::builtins::ImageViewTool));
    tools.register(Arc::new(reflect_tools::builtins::NotebookEditTool));
    // P2 `brief-tool`:把附件/长上下文整理成简短 briefing。工具结构此前已
    // 实现但未注册到 registry(agent 无法调用),此处接线后 agent 可主动调用。
    tools.register(Arc::new(reflect_tools::builtins::BriefTool));
    // v1.2 P1-12:`get_context_remaining` —— 让 agent 主动查会话 token 用量 /
    // 上下文窗口 / 预算剩余(只读、Auto 权限)。数据源与 `model_call` 共享
    // `session_usage` / `context_window_size` / `token_budget` 句柄。
    tools.register(Arc::new(reflect_tools::builtins::GetContextRemainingTool));
    // v1.2 P0-3:checkpoint / rewind —— git 快照与回退工具对。checkpoint
    // 拍工作区快 sha(走 Prompt),rewind 恢复文件树(High 风险,走 Prompt +
    // approval gate)。只回退工作区,会话历史 append-only 保留。
    tools.register(Arc::new(crate::CheckpointTool));
    tools.register(Arc::new(crate::RewindTool));
    // v1.1.0: task 系统 + TodoWrite —— 7 个 builtin tools,
    // 由 TaskManager 统一调度,钩子集成通过 TaskManager.fire_task_*
    // 路径在 tool execute 内部派发。
    //
    // **S2.5 起**:默认走 `FileTaskStore::with_default_home()` 持久化,
    // 与 CLI 的 `reflect task ls` 同源(`~/.reflect/tasks/<list>/<id>.json`)。
    // 测试 / 一次性场景用 `--ephemeral-tasks` flag 切回内存后端。
    // Fallback: 若 `$HOME` 未设(罕见,如容器 / 单元测试),降级到内存。
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
    // v1.x:TaskManager 接入共享 HookEngine,让 task 生命周期事件
    // (TaskCreated / TaskCompleted / TaskUpdated)走与 AgentThread 同一份
    // hook engine,用户配的 `[hooks]` 能看到任务事件。此前
    // `with_hook_engine` 是孤儿 builder,manager 内 hook_engine 恒为 None。
    // `with_event_sink` 暂不接 —— EventMsg 无 Task* 变体,接了也是死代码。
    let task_manager = Arc::new(
        reflect_task::TaskManager::new(task_store, team_store)
            .with_hook_engine(hook_engine.clone()),
    );
    reflect_task::register_task_tools(&tools, task_manager.clone());
    bootstrap::TOOLS.with(|t| *t.borrow_mut() = Some(tools.clone()));

    // 3. AgentThread + Ctrl-C 处理器
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
    // `--auto-root`:从 cwd 向上探测项目根。默认关;探测失败原样用 cwd。
    let workspace = if args.auto_root {
        reflect_core::detect_project_root(&workspace)
    } else {
        workspace
    };

    // ── resume 分支 ──
    // 三种入口汇成同一条 resume 路径:
    //   - `--resume <UUID>`         → 直接用 UUID
    //   - `-c` / `--continue-last`  → 用 list_sessions[0](newest)
    //   - `-r N` / `--resume-by N`  → 用 list_sessions[N-1]
    // 这里先决定 `resume_thread_id_str`,后续共用同一段 bootstrap_resume。
    let resume_thread_id_str: Option<String> = if let Some(s) = args.resume.as_deref() {
        Some(s.to_string())
    } else if args.continue_last || args.resume_by.is_some() {
        let base = reflect_rollout::path::default_base();
        let tid = reflect_rollout::index::resolve_session_index(
            &base,
            args.continue_last,
            args.resume_by,
        )?;
        Some(tid.to_string())
    } else {
        None
    };
    if let Some(thread_id_str) = resume_thread_id_str.as_deref() {
        let bundle = bootstrap_resume(thread_id_str).await?;
        // 构造最小的 M4Deps(不带 compactor / memory —— 恢复的 session
        // 通过回放补足先前状态,不需要再做 compaction)。
        let resume_recorder: Arc<dyn reflect_protocol::RolloutRecorder> = Arc::new(
            JsonlRolloutWriter::new(reflect_rollout::path::default_base(), bundle.thread_id),
        );
        let mut m4 =
            reflect_core::config::default_m4_deps(args.agent.as_deref().unwrap_or("default"));
        m4.recorder = Some(resume_recorder.clone());
        let cfg = AgentConfig::new(model.clone(), workspace.clone())
            .with_m4(m4)
            .with_policy(Arc::new(initial_cfg.routing_policy()))
            .with_telemetry(build_telemetry_sink(&initial_cfg))
            // Ctrl-C 取消令牌必须接入 AgentConfig —— 否则上面 spawn 的
            // ctrl_c() 处理器 cancel 的是一个无人持有的令牌,运行中的 turn
            // (LLM 流 / 工具执行)不会被打断,Ctrl-C 形同虚设。
            .with_cancel(cancel.clone())
            // 把回放出的历史消息喂回 agent:submission_loop 会在首个 turn
            // 把它们前置到当前用户输入之前,恢复的 agent 因此记得之前的对话。
            // 此前这里只用了 `bundle.initial_messages.len()` 拼一条
            // system-reminder,真正的历史被丢弃 —— 恢复后 agent 毫无记忆。
            .with_preload_messages(bundle.initial_messages.clone());
        let cfg_for_reload = cfg.clone();
        let thread = AgentThread::new(
            cfg,
            registry.clone(),
            tools.clone(),
            Some(sanitizer.clone()),
            // v1.x:注入从 config.toml `[hooks]` 构建的 HookEngine。
            Some(hook_engine.clone()),
        );
        // v0.3: resume 分支也跑 MCP bootstrap,方便用户继续使用之前挂的
        // MCP server。subagent factory 仍为 None(resume 不跑 bootstrap_m5)。
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
            tools,
            None,
        );

        let prompt = format!(
            "<system-reminder>Resumed session {} ({} prior messages)</system-reminder>\n\nContinue.",
            bundle.thread_id,
            bundle.initial_messages.len()
        );
        let sub = Submission::user_input(prompt);
        let mut handle = thread.submit(sub).await;
        let stdout = std::io::stdout();
        let mut writer = JsonlWriter::new(stdout.lock());
        while let Some(event) = handle.next().await {
            if let Err(e) = writer.write_event(&event) {
                tracing::warn!(error = %e, "jsonl writer failed; exiting");
                break;
            }
        }
        return Ok(());
    }

    // ── 普通分支(非 resume)──
    let prompt = args
        .prompt
        .ok_or_else(|| anyhow::anyhow!("usage: reflect exec <prompt>"))?;

    // 4. M4 bootstrap:加载 agent 定义、skills,构造压缩器 /
    //    记忆 / prompt 构建器。
    // v1.2 P1:telemetry sink 在所有 bootstrap 之前构造一次,复用给 m4
    // (summarizer)、m5 (subagent factory)、AgentConfig,保证 session_id
    // 一致且不重复构造。
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
    // v1.0 多 Provider 路由:把 `RoutingPolicy` 注入 `AgentConfig`,
    // `model_call` 入口会读 `ctx.policy.resolve(Role::Main)` 拿 spec。
    let policy = Arc::new(initial_cfg.routing_policy());

    // 5. M5 bootstrap:注册 subagent 工具(call_<role>)。
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
    // v1.2 P1:把 telemetry sink 注入 subagent factory,让子 agent 的
    // model_call 也复用 reflect-core 既有落库逻辑。
    factory.set_telemetry(telemetry_sink.clone());

    // v1.1.0 Phase 4:Coordinator 模式整合 ── 在 `factory` 构造后:
    //   1) 把 team members spec 注入 factory(若已加载);
    //   2) 在 scratchpad 路径建目录;
    //   3) `apply_coordinator_from_config` 统一开关 factory / 工具 / prompt。
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
    // v1.2 P1-12:从 `[token_budget].session_total_tokens` / env
    // `REFLECT_TOKEN_BUDGET` 解析会话预算上限。`None` = 仅靠 max_iterations。
    let token_budget = reflect_core::config::token_budget_from_env(
        initial_cfg
            .token_budget
            .as_ref()
            .and_then(|s| s.session_total_tokens),
    );
    // v1.x:从 `active.max_iterations` / env `REFLECT_MAX_ITERATIONS` 解析
    // 全局 agent 主循环迭代上限;缺省回退默认 32(向后兼容)。
    let max_iterations =
        reflect_core::config::max_iterations_from_env(initial_cfg.active.max_iterations);
    // v1.x 功能 7:收集所有声明了 quota 的 credential,注册到 QuotaTracker。
    // 任一 provider 有 quota 声明即构造 tracker;否则传 None(向后兼容)。
    let quota_tracker = build_quota_tracker(&initial_cfg);
    // web_search 工具级 env:从 `[web_search].api_key` 注入到 ToolContext.env,
    // 让 web_search 读到 BRAVE_API_KEY。空字符串 / 缺省不注入(工具会再
    // fallback 到 std::env::var("BRAVE_API_KEY"))。
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
        // Ctrl-C 取消令牌必须接入 AgentConfig(同 resume 分支)。
        .with_cancel(cancel.clone())
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
        // 与 TUI 路径对齐:config.toml 的 [permissions] 规则(只读,优先匹配)
        // + file store(可写,持久化)组成 ChainedPermissionStore。
        let file_store: Arc<dyn reflect_permissions::PermissionStore> = Arc::new(file_store);
        let cfg_store: Arc<dyn reflect_permissions::PermissionStore> =
            Arc::new(reflect_permissions::InMemoryPermissionStore::new());
        if let Some(sec) = &initial_cfg.permissions {
            // 展平 deny + allow 紧凑数组 + 显式 rules 为统一规则列表
            // (Claude Code 式语法糖见 PermissionsSection::expanded_rules)。
            //
            // 修复 bug:此前 `let _ = cfg_store.add(rule.clone())` 把返回的
            // future 直接丢弃 —— `PermissionStore::add` 是 async fn,未 await
            // 意味着规则**从未真正写入** store,config.toml [permissions] 段的
            // 全部 deny/allow 规则静默失效。改为逐条 `.await`,失败仅 warn
            // (单条规则落库失败不阻塞启动,与其它 best-effort 路径一致)。
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
    // P2 `yolo-classifier`:默认注入 HeuristicYoloClassifier(只读工具 Allow /
    // 破坏性 bash Ask)。此前分类器 trait + 启发式实现已存在但未接入 ApprovalGate
    // (Auto 模式落到 modal);此处接线后 Auto 模式按启发式建议自动批准 / 拒绝 /
    // 问询。v2 可换 LLM 后端。
    cfg.yolo_classifier = Some(Arc::new(reflect_permissions::HeuristicYoloClassifier));
    let cfg_for_reload = cfg.clone(); // 共享 model Arc<RwLock>
    let thread = AgentThread::new(
        cfg,
        registry.clone(),
        tools.clone(),
        Some(sanitizer.clone()),
        Some(hook_engine.clone()),
    );

    // v1.2 P1-2:Cron 真实调度。取 agent loop 的 submission sender 注入
    // `CronScheduler`,后台 driver 到期时把 job.prompt 作为
    // `Submission::user_input` 注入触发新一轮对话。session 维度持久化 /
    // 调度(`thread_id`)。注册 `CronTool` 让 agent 可创建 / 列出 / 查询 /
    // 修改 / 删除定时任务。
    let cron_scheduler = Arc::new(reflect_stream::CronScheduler::new(
        Some(thread.submission_sender()),
        thread_id,
    ));
    tools.register_with_source(
        reflect_tools::ToolSource::Builtin,
        Arc::new(crate::CronTool::new(cron_scheduler.clone())),
    );
    // 启动 driver:每 30s 扫描到期 job。`start` 返回的 handle 一旦 drop 会
    // abort 底层 task,故 move 进一个 detached task 让它活到进程退出。
    // `(*cron_scheduler).clone()` 得到一个 `CronScheduler`(共享内部 jobs Arc),
    // 因为 `start(self, ..)` 取所有权。
    let cron_driver = (*cron_scheduler).clone().start(30);
    tokio::spawn(async move {
        // `_handle` 持有 driver 句柄,此 task 不结束即不 Drop。
        let _handle = cron_driver;
        std::future::pending::<()>().await;
    });
    tracing::info!(session_id = %thread_id, "cron scheduler started (tick=30s)");

    // v1.x Plan mode:注册 `PlanModeGate` builtin hook 到默认 hook 引擎。
    // 该 hook 在 `PermissionMode::Plan` 下 blanket-deny 白名单外工具
    // (默认白名单 = `read` / `grep` / `glob` / `echo` + Plan 控制面工具)。
    thread.register_hook(PlanModeGate::default_mode());

    // v1.0.0-rc3+:`read_before_edit` hook —— 拦截 `write`/`edit`/
    // `notebook_edit` 的写子动作,要求本 session 内 agent 先 `read` 过
    // 目标文件(否则 Deny)。`bash` 工具 v1 不拦截(plan R2:防误覆盖,
    // 非安全边界)。通过 `build_read_before_edit` 一站式工厂统一 CLI
    // 与 TUI 的注册路径(review bug-5)。返回的 `state` Arc 由
    // hook 持有;`read` tool 通过 PostToolUse 写入。
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
        // 不 emit 任何 JSONL event —— headless 模式下用户已经在 CLI 看到
        // banner;真正的 plan lifecycle 由后续 PlanRequest / PlanReady 事件驱动。
    }

    // v0.3: MCP bootstrap —— 启动 [mcp_servers] 中所有 server,把 tool
    // adapter 注册到 ToolRegistry。失败仅 warn,不阻塞主流程。
    let mcp_manager = bootstrap_m6(&initial_cfg, reload_tx.clone()).await;

    // v0.5: LSP bootstrap —— 启动 [lsp_servers] 中所有 server,注册
    // 单例 `lsp` tool。失败仅 warn,不阻塞主流程。
    let _lsp_manager = bootstrap_lsp(&initial_cfg, reload_tx.clone(), tools.clone()).await;

    // v1.0.0-rc2 Phase B:插件 runtime —— 挂载 enabled_plugins 能力。
    let mcp_for_plugins = mcp_manager.clone().unwrap_or_else(|| {
        let (tx, _rx) = tokio::sync::mpsc::channel::<reflect_mcp::McpLifecycleEvent>(16);
        Arc::new(McpConnectionManager::new(tx))
    });

    // v1.2.0 P2 `mcp-resources`:把 ListMcpResources / ReadMcpResource 工具
    // 注册到 registry。此前工具结构已存在但未注册且 execute 返回 stub,
    // 现在真正调用 rmcp peer 的 list_all_resources / read_resource。
    // 复用 mcp_for_plugins(若 [mcp_servers] 为空则是一个空 manager,
    // 工具会优雅返回 "no resources")。在 bootstrap_plugins 消费所有权前
    // 先 clone,供后续注册使用。
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
        // 批次二十四(#5):emit PluginLoaded 给 JSONL drainer。
        Some(reload_tx.clone()),
    )
    .await;

    // v0.2.2: 在 AgentThread 构造完成后再启动 reload task —— 需要
    // 把 `cfg_for_reload` 和 `factory` 一起传进去,这样 handle_reload
    // 才能同步更新 model 与 subagent default。
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
        tools,
        Some(plugin_runtime),
    );

    // 4. 提交并以 JSONL 流式输出事件。M8 P1b:一个专用 OS 线程
    // 排空 `reload_rx` channel,把 `ConfigReloaded` 事件随到随写到
    // stdout。主回合循环与 drainer 每次写都各自获取新的
    // `StdoutLock` —— `StdoutLock` 是 `!Send`,无法共享;但按次加锁
    // 很廉价,且 reload 事件到达率可忽略(每次保存 ≤ 1 条)。
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<reflect_protocol::Event>();
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
    // 桥接:把 async 的 `reload_rx` 转发到 sync 端。该桥接 task 在
    // `reload_rx` 关闭时退出(即 `reload_tx` 在 `async_main` 末尾被 drop 时)。
    tokio::spawn(async move {
        let mut rx = reload_rx;
        while let Some(event) = rx.recv().await {
            if sync_tx.send(event).is_err() {
                break;
            }
        }
    });
    let sub = Submission::user_input(prompt);
    let mut handle = thread.submit(sub).await;
    while let Some(event) = handle.next().await {
        let mut w = JsonlWriter::new(std::io::stdout().lock());
        if let Err(e) = w.write_event(&event) {
            // broken pipe(如 | head 已关闭)—— 优雅退出
            tracing::warn!(error = %e, "jsonl writer failed; exiting");
            break;
        }
    }
    Ok(())
}
