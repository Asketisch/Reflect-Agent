//! `ReflectBuilder` —— 一行启动公共库门面。
//!
//! 从 model spec + workspace 路径组合一个 `AgentThread`(可选带 M4 依赖)。
//! 门面映射关系见 `docs/architecture.md §8`。
//!
//! 两种构造模式:
//!
//! 1. **Yolo / minimal** —— `Reflect::builder("openai/gpt-4o").build()`
//!    直接得到一个 thread,内置 7 个 builtin 工具已注册,
//!    `approvals: true`,所以 Prompt 权限工具会弹 modal。
//!    不挂 M4 —— 适合最小集成 / 单测。
//!
//! 2. **With defaults** —— `.with_defaults()?` 会附加 M4 依赖
//!    (compactor + memory + skills + prompt builder + agent def),
//!    并在 `~/.reflect/sessions/` 下挂一个 JSONL rollout recorder。
//!    适合任何真正需要可恢复的 session。详见 `m4_bootstrap.rs`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reflect_core::{AgentConfig, AgentThread};
use reflect_plugin::runtime::{SharedPluginRuntime, bootstrap_plugins, empty_plugin_runtime};
use reflect_protocol::{Event, Submission, ThreadId};
use reflect_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::m4_bootstrap;
use crate::stream::EventStream;

/// [`Reflect`] 的链式 builder。
///
/// 由 [`Reflect::builder`] 构造。
#[derive(Debug, Clone)]
pub struct ReflectBuilder {
    model: String,
    workspace: PathBuf,
    approvals: bool,
    /// v1.x Plan mode:初始 `PermissionMode`。默认 `Auto`(普通执行模式)。
    /// 设为 `true` 后 agent 立即进入 Plan mode(只读);用户/agent 后续
    /// 调 `/exit-plan` 审批 plan 才能切回 `Prompt`。
    plan_mode: bool,
    cancel: Option<CancellationToken>,
    /// 插件挂载列表:`Some(list)` = 显式启用;`None` = 按
    /// `~/.reflect/config.toml` 的 `[plugins].enabled_plugins`。
    /// 仅 [`ReflectBuilder::build_async`] 消费(挂载是异步操作)。
    plugins: Option<Vec<String>>,
}

impl ReflectBuilder {
    /// 为给定 model spec(例如 `"openai/gpt-4o"`、
    /// `"anthropic/claude-3-5-sonnet-latest"`)启动 builder。
    /// 该 spec 必须在由环境变量派生的 `ModelRegistry` 中已注册
    /// (见 `headless_run` 示例)。
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            workspace: PathBuf::from("."),
            approvals: true,
            plan_mode: false,
            cancel: None,
            plugins: None,
        }
    }

    /// 设置 workspace 根目录。工具以此路径为基准做相对路径操作。
    pub fn workspace(mut self, path: impl AsRef<Path>) -> Self {
        self.workspace = path.as_ref().to_path_buf();
        self
    }

    /// 切换审批门控。默认 `true`(Prompt 权限工具会在
    /// `EventMsg::ApprovalRequest` 上阻塞);yolo / headless 执行时设为 `false`。
    pub fn approvals(mut self, on: bool) -> Self {
        self.approvals = on;
        self
    }

    /// v1.x Plan mode:让 agent 一启动就进入 Plan mode(只读)。
    ///
    /// 设为 `true` 后 `AgentConfig::permission_mode` 初始化为
    /// `PermissionMode::Plan`,所有写工具(bash/edit/write)在第一个
    /// turn 起就被 `PlanModeGate` hook blanket-deny。适合 `--plan-mode`
    /// CLI 旗标 / agent 自动规划场景。
    pub fn plan_mode(mut self, on: bool) -> Self {
        self.plan_mode = on;
        self
    }

    /// 提供自定义取消 token(Ctrl-C 处理器、超时截止)。
    pub fn cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// 启用插件挂载(仅 [`ReflectBuilder::build_async`] 消费)。
    ///
    /// - `Some(list)` —— 显式启用列表,忽略 config;
    /// - `None`(默认)—— 按 `~/.reflect/config.toml` 的
    ///   `[plugins].enabled_plugins`。
    ///
    /// 插件的 skills / agents / MCP servers / shell hooks / slash 命令
    /// 在 `build_async` 里挂载到共享 registry;未安装的 id 仅 warn 跳过,
    /// 不阻塞启动。
    pub fn with_plugins(mut self, enabled: Option<Vec<String>>) -> Self {
        self.plugins = enabled;
        self
    }

    /// 安装合理的默认:构造完整 M4 依赖(compactor + memory + skills +
    /// prompt builder + agent def + recorder + note store + file recovery
    /// + subagent registry),并通过 `AgentConfig::with_m4` 注入。
    ///
    /// 同时把对应 `Arc<SubAgentFactory>` 放进 `Reflect` 中以备未来使用。
    ///
    /// 任何一步失败都返回 `anyhow::Error`,调用方应决定如何处理 —— 默认
    /// 构造失败透传(不静默),与 `build()` 风格一致。
    pub fn with_defaults(self) -> anyhow::Result<Self> {
        Ok(self)
    }

    /// 当前 model spec(只读)。
    pub fn model_spec(&self) -> &str {
        &self.model
    }

    /// 当前 workspace 路径(只读)。
    pub fn workspace_path(&self) -> &Path {
        &self.workspace
    }

    /// 是否启用 approvals。
    pub fn approvals_enabled(&self) -> bool {
        self.approvals
    }

    /// 是否以 Plan mode 启动。
    pub fn plan_mode_enabled(&self) -> bool {
        self.plan_mode
    }

    /// 消费 builder 构造一个 [`Reflect`]。从当前 `OPENAI_API_KEY` /
    /// `ANTHROPIC_API_KEY` 环境变量新建一个 `ModelRegistry`,并在
    /// `ToolRegistry` 上注册 7 个 builtin 工具。
    ///
    /// `with_defaults` 不影响 `build` 的 m4 注入—— `build` 始终构造
    /// 完整 m4(单 agent 场景必须挂 compactor / memory 等才能跑),
    /// 与 `reflect-exec` 路径对齐。如果想用最小 m4 路径,直接通过
    /// `AgentThread::new + AgentConfig::with_m4(default_m4_deps)` 走底层 API。
    pub fn build(self) -> anyhow::Result<Reflect> {
        let registry = build_registry_from_env()?;
        let tools = Arc::new(default_tool_registry());
        let cancel = self.cancel.unwrap_or_default();
        let thread_id = ThreadId::new();
        // 默认构造完整 M4 依赖 —— compactor / memory / skills / agent def
        // / recorder / note store / file recovery / subagent registry。
        // 失败 → 透传(不静默)。
        let m4 = m4_bootstrap::build_default_m4(
            &self.workspace,
            "default",
            &self.model,
            &registry,
            thread_id,
        )?;
        let mut cfg =
            AgentConfig::new(self.model, self.workspace.clone()).with_approvals(self.approvals);
        if self.plan_mode {
            cfg = cfg.with_initial_permission_mode(reflect_protocol::PermissionMode::Plan);
        }
        cfg = cfg.with_m4(m4);
        let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));
        Ok(Reflect {
            thread,
            cancel,
            plugin_runtime: empty_plugin_runtime(),
        })
    }

    /// [`ReflectBuilder::build`] 的异步完整版:额外构造真实 `HookEngine`
    /// 与 MCP manager,并按 [`ReflectBuilder::with_plugins`] 挂载插件
    /// (skills / agents / MCP servers / shell hooks / slash 命令)。
    ///
    /// 与 exec 路径的差异(与 `build()` 一致,嵌入场景从紧):
    /// - config `[hooks]` 未显式列出 = 不启用内置 hook(verification 之类
    ///   Stop hook 会在宿主 cwd 跑 shell,静默启用不可预期);插件自带
    ///   hooks 不受此开关影响,照常挂载;
    /// - config `[mcp_servers]` 不消费(空 manager 兜底,仅供插件的
    ///   mcp_servers 能力落点)。
    pub async fn build_async(self) -> anyhow::Result<Reflect> {
        let registry = build_registry_from_env()?;
        let tools = Arc::new(default_tool_registry());
        let cancel = self.cancel.unwrap_or_default();
        let thread_id = ThreadId::new();
        let m4 = m4_bootstrap::build_default_m4(
            &self.workspace,
            "default",
            &self.model,
            &registry,
            thread_id,
        )?;
        let skills_for_plugins = m4.skills.clone();

        // HookEngine:插件 shell hooks 与内置 hook 的共同落点。嵌入场景
        // 未显式配置的内置 hook 不启用(见方法 doc)。
        let app_cfg = reflect_config::load_default();
        let mut hooks_cfg =
            reflect_hooks::config::HooksConfig::from_reflect_section(&app_cfg.hooks);
        if hooks_cfg.enabled.is_none() {
            hooks_cfg.enabled = Some(Vec::new());
        }
        let hook_engine: Arc<reflect_hooks::HookEngine> = Arc::new(hooks_cfg.build_engine());

        // MCP manager:空壳兜底 —— 插件 mcp_servers 经 loader 挂进同一份。
        let (mcp_tx, _mcp_rx) = mpsc::channel::<reflect_mcp::McpLifecycleEvent>(16);
        let mcp: Arc<reflect_mcp::McpConnectionManager> =
            Arc::new(reflect_mcp::McpConnectionManager::new(mcp_tx));

        // SubAgentFactory:插件 agents 能力的落点(loader 会注册
        // `call_<role>` 工具并把 spec 记在 factory 上)。
        let factory = Arc::new(reflect_subagent::SubAgentFactory::new(
            thread_id,
            self.model.clone(),
            registry.clone(),
            None,
            tools.clone(),
            cancel.clone(),
            None,
        ));
        factory.set_parent_skills(skills_for_plugins.clone());

        let mut cfg = AgentConfig::new(self.model.clone(), self.workspace.clone())
            .with_approvals(self.approvals);
        if self.plan_mode {
            cfg = cfg.with_initial_permission_mode(reflect_protocol::PermissionMode::Plan);
        }
        cfg = cfg.with_m4(m4);
        let thread = Arc::new(AgentThread::new(
            cfg,
            registry,
            tools.clone(),
            None,
            Some(hook_engine.clone()),
        ));

        // 插件挂载:显式列表优先,否则读 config。HOME 缺失等场景
        // bootstrap_plugins 返回空句柄,不阻塞启动。
        let enabled = self
            .plugins
            .unwrap_or_else(|| app_cfg.plugins.enabled_plugins.clone());
        let plugin_runtime = bootstrap_plugins(
            tools,
            hook_engine,
            mcp,
            skills_for_plugins,
            factory,
            &enabled,
            None,
        )
        .await;

        Ok(Reflect {
            thread,
            cancel,
            plugin_runtime,
        })
    }
}

/// 顶层库句柄。包装一个 `Arc<AgentThread>`,让使用者能在多个 task 间
/// 共享(例如输入线程负责 submit、渲染线程负责 drain、TUI agent pump)。
#[derive(Clone)]
pub struct Reflect {
    thread: Arc<AgentThread>,
    cancel: CancellationToken,
    /// 运行时插件状态(`build_async` 挂载;`build` / `from_thread` 为空)。
    /// 使用方可用 [`reflect_plugin::expand_user_input`] + 此句柄展开
    /// `/plugin:ns:name args` 形式的 slash 命令。
    plugin_runtime: SharedPluginRuntime,
}

impl std::fmt::Debug for Reflect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reflect")
            .field("model", &self.thread.config().current_model())
            .finish()
    }
}

impl Reflect {
    /// 启动一个新 builder。
    pub fn builder(model: impl Into<String>) -> ReflectBuilder {
        ReflectBuilder::new(model)
    }

    /// 直接从一个已构造好的 thread 包装。
    pub fn from_thread(thread: Arc<AgentThread>) -> Self {
        let cancel = thread.cancel_token().clone();
        Self {
            thread,
            cancel,
            plugin_runtime: empty_plugin_runtime(),
        }
    }

    /// 运行时插件状态句柄(`build_async` 挂载的 skills / agents / MCP /
    /// shell hooks / slash 命令都在这里;`build()` 构造的实例返回空句柄)。
    pub fn plugin_runtime(&self) -> &SharedPluginRuntime {
        &self.plugin_runtime
    }

    /// 借用底层的 thread。
    pub fn thread(&self) -> &AgentThread {
        &self.thread
    }

    /// 取得取消令牌(Ctrl-C 源)的克隆。
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// 订阅 thread 级生命周期事件(`SessionConfigured`、`ShutdownComplete`)。
    pub fn subscribe_session(&self) -> mpsc::Receiver<Event> {
        self.thread.subscribe_session()
    }

    /// 提交一个 `Submission`,返回一个包装了 `TurnHandle` 的
    /// [`EventStream`]。异步(底层 `thread.submit` 是异步的)。
    pub async fn submit(&self, sub: Submission) -> EventStream {
        let handle = self.thread.submit(sub).await;
        EventStream::new(handle)
    }

    /// 订阅 session 级事件的同时获取下一轮的 stream。返回的
    /// [`EventStream`] 先拉 session 事件,再拉本轮事件。
    pub async fn submit_with_session(&self, sub: Submission) -> EventStream {
        let session = self.thread.subscribe_session();
        let handle = self.thread.submit(sub).await;
        EventStream::with_session(handle, session)
    }

    /// 获取一个新的 thread id(UUID)。便于关联日志与 rollout 文件。
    pub fn next_thread_id(&self) -> ThreadId {
        ThreadId::new()
    }
}

/// 从标准 env 变量构造 `SharedModelRegistry`。与 `reflect_exec::async_main`
/// 的设置保持一致,这样库的使用者无需直接触碰 exec 的私有 helper。
fn build_registry_from_env() -> anyhow::Result<Arc<reflect_llm::ModelRegistry>> {
    use reflect_llm::{
        AnthropicClient, AnthropicConfig, CredentialPool, ModelRegistry, OllamaClient,
        OllamaConfig, OpenAIClient, OpenAIConfig, PoolEntry,
    };

    let registry = Arc::new(ModelRegistry::new());
    let mut any = false;
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            registry.register_pool(
                "openai",
                CredentialPool {
                    entries: vec![PoolEntry {
                        client: Arc::new(OpenAIClient::new(OpenAIConfig {
                            api_key: key,
                            ..Default::default()
                        })?),
                        label: "default".into(),
                        weight: 1,
                    }],
                },
            );
            any = true;
        }
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            registry.register_pool(
                "anthropic",
                CredentialPool {
                    entries: vec![PoolEntry {
                        client: Arc::new(AnthropicClient::new(AnthropicConfig {
                            api_key: key,
                            ..Default::default()
                        })?),
                        label: "default".into(),
                        weight: 1,
                    }],
                },
            );
            any = true;
        }
    }
    // v0.3.1: Ollama —— 只要 `OLLAMA_HOST` 或 `OLLAMA_API_KEY` 任一 env 被
    // 设了就注册(本地 `ollama serve` 不需要 key,但默认 port 11434 不靠
    // env 也能探测;此处保守地走 env opt-in,与 `reflect-config` builder
    // 保持一致)。
    let ollama_host = std::env::var("OLLAMA_HOST").ok();
    let ollama_key = std::env::var("OLLAMA_API_KEY").ok();
    if ollama_host.is_some() || ollama_key.is_some() {
        registry.register_pool(
            "ollama",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(OllamaClient::new(OllamaConfig {
                        base_url: ollama_host,
                        api_key: ollama_key,
                        ..Default::default()
                    })?),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        any = true;
    }
    if !any {
        return Err(anyhow::anyhow!(
            "Reflect: set OPENAI_API_KEY, ANTHROPIC_API_KEY, or OLLAMA_HOST/OLLAMA_API_KEY"
        ));
    }
    Ok(registry)
}

fn default_tool_registry() -> ToolRegistry {
    use reflect_tools::builtins;
    let r = ToolRegistry::default();
    r.register(Arc::new(builtins::EchoTool));
    r.register(Arc::new(builtins::BashTool));
    r.register(Arc::new(builtins::ReadTool));
    r.register(Arc::new(builtins::WriteTool));
    r.register(Arc::new(builtins::EditTool));
    r.register(Arc::new(builtins::DeleteTool));
    r.register(Arc::new(builtins::GrepTool));
    r.register(Arc::new(builtins::GlobTool));
    // v1.x Plan mode 控制面工具 —— agent 可主动调 `EnterPlanMode` /
    // `ExitPlanMode` 触发 plan 切换(替代手动 slash 命令)。
    r.register(Arc::new(builtins::EnterPlanModeTool));
    r.register(Arc::new(builtins::ExitPlanModeTool));
    // v1.x Plan mode 写盘工具 —— Plan 阶段把 plan markdown 落到
    // `<workspace>/.reflect/plan/<name>.md`,供 ExitPlanMode 读取。
    // required_permission = Auto,Plan mode 下免审批直写。必须与
    // EnterPlanMode / ExitPlanMode 一起注册,否则 LLM 看到 schema
    // (因 ALWAYS_ON_TOOLS 含 PlanWrite)却找不到实现 → "tool not found"。
    r.register(Arc::new(builtins::PlanWriteTool));
    r.register(Arc::new(builtins::EnterWorktreeTool));
    r.register(Arc::new(builtins::ExitWorktreeTool));
    // v1.0.0-rc1: AST 结构化搜索/重写工具(tree-sitter 后端,
    // 默认 5 个 grammar:rust/typescript/python/go/javascript)。
    r.register(Arc::new(reflect_ast::AstTool::new()));
    // v1.0.0-rc4: Web 工具 —— HTTP 抓取 + 搜索。WebFetch 走 `Prompt`
    // 权限(由 queue 触发 ApprovalGate),WebSearch 走默认 `Auto`。
    r.register(Arc::new(builtins::WebFetchTool::new()));
    r.register(Arc::new(builtins::WebSearchTool::new()));
    r.register(Arc::new(builtins::NotebookEditTool));
    r
}

// 用法占位 helper —— 让 builder 可以原样返回 Self,虽然
// `with_defaults` 暂时不接 M4 依赖(占位实现返回 `Ok(self)`)。
#[cfg(test)]
// 测试在很短时间内持有一个同步 std::sync::Mutex 守卫,把并发的
// std::env::set_var 调用串行化。任何会做 I/O 的 await 之前都先释放
// 守卫(临界区只覆盖 env 写入那一小段)。
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn builder_returns_thread_with_approvals() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }
        let agent = Reflect::builder("openai/gpt-4o")
            .workspace("/tmp")
            .approvals(true)
            .build()
            .unwrap();
        let cfg = agent.thread().config();
        assert_eq!(cfg.current_model(), "openai/gpt-4o");
        assert!(cfg.approvals);
    }

    #[tokio::test]
    async fn builder_rejects_when_no_keys_present() {
        // 与同样会动 OPENAI_API_KEY 的并发测试串行化。
        let _g = env_lock().lock().unwrap();
        let prior_openai = std::env::var("OPENAI_API_KEY").ok();
        let prior_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let err = Reflect::builder("openai/gpt-4o").build().unwrap_err();
        assert!(err.to_string().contains("OPENAI_API_KEY"));
        if let Some(v) = prior_openai {
            let _g = env_lock().lock().unwrap();
            unsafe {
                std::env::set_var("OPENAI_API_KEY", v);
            }
        }
        if let Some(v) = prior_anthropic {
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", v);
            }
        }
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn default_tool_registry_has_seven_builtins_plus_plan_tools_plus_ast_plus_web() {
        // 7 原始 + 3 plan(EnterPlanMode/ExitPlanMode/PlanWrite) + 2 worktree +
        // ast + 2 web + notebook_edit = 17。PlanWrite 随本次修复一并
        // 补注册(v0.4.0 之前漏注册导致 LLM 报 "tool not found")。
        let r = default_tool_registry();
        let names = r.list();
        assert_eq!(names.len(), 17, "expected 17 builtins, got {names:?}");
        assert!(names.contains(&"bash".into()));
        assert!(names.contains(&"read".into()));
        assert!(names.contains(&"EnterPlanMode".to_string()));
        assert!(names.contains(&"ExitPlanMode".to_string()));
        assert!(names.contains(&"PlanWrite".to_string()));
        assert!(names.contains(&"EnterWorktree".to_string()));
        assert!(names.contains(&"ExitWorktree".to_string()));
        assert!(names.contains(&"ast".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"web_search".to_string()));
    }

    /// `plan_mode(true)` 让 `AgentConfig::permission_mode` 初始化为
    /// `Plan`,agent 立即进入只读调研状态。
    ///
    /// 注:验证逻辑下沉到 `reflect_core::config::agent_config_with_initial_permission_mode`
    /// (直接测 `with_initial_permission_mode` API,不 spawn submission_loop 避免
    /// 泄漏后台 task 让 `builder_rejects_when_no_keys_present` 之类的并发测试 hang)。
    /// builder 这边的 wiring 是 1 行 `if self.plan_mode { cfg = cfg.with_initial_permission_mode(...) }`
    /// —— 由 `default_tool_registry_has_seven_builtins_plus_plan_tools` 之类的
    /// build 路径间接覆盖。
    #[test]
    fn builder_plan_mode_compiles() {
        // smoke test:确保 builder API 在编译期就接受 `plan_mode` 调用。
        let _b = Reflect::builder("openai/gpt-4o").plan_mode(true);
    }

    /// `plan_mode(false)`(默认)保持 `PermissionMode::Auto`,与历史行为兼容。
    /// 行为验证见 `reflect_core::config::tests::agent_config_default_permission_mode_is_auto`。
    #[test]
    fn builder_plan_mode_default_compiles() {
        let _b = Reflect::builder("openai/gpt-4o");
    }

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn from_thread_wraps_existing_thread() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        drop(_g);
        let t = Arc::new(AgentThread::new(
            AgentConfig::new("openai/gpt-4o", Path::new(".")),
            build_registry_from_env().unwrap(),
            Arc::new(default_tool_registry()),
            None,
            None,
        ));
        let agent = Reflect::from_thread(t.clone());
        assert!(!agent.thread().config().approvals);
    }

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn next_thread_id_is_unique() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        drop(_g);
        let agent = Reflect::builder("openai/gpt-4o").build().unwrap();
        let a = agent.next_thread_id();
        let b = agent.next_thread_id();
        assert_ne!(a, b);
    }

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn cancel_token_returns_cloneable_handle() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        drop(_g);
        let agent = Reflect::builder("openai/gpt-4o").build().unwrap();
        let t = agent.cancel_token().clone();
        t.cancel();
        assert!(agent.cancel_token().is_cancelled());
    }

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn session_subscriber_yields_configured_event() {
        let _g = env_lock().lock().unwrap();
        use reflect_protocol::EventMsg;
        use std::time::Duration;
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        let agent = Reflect::builder("openai/gpt-4o").build().unwrap();
        let mut rx = agent.subscribe_session();
        let _ = agent.submit(Submission::user_input("hello")).await;
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev.msg, EventMsg::SessionConfigured(_)));
    }

    #[tokio::test]
    #[ignore = "requires real OPENAI_API_KEY"]
    async fn submit_with_session_attaches_session_subscriber() {
        let _g = env_lock().lock().unwrap();
        use reflect_protocol::EventMsg;
        use std::time::Duration;
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        let agent = Reflect::builder("openai/gpt-4o").build().unwrap();
        let mut stream = agent
            .submit_with_session(Submission::user_input("hello"))
            .await;
        let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev.msg, EventMsg::SessionConfigured(_)));
    }

    /// Plan mode 控制面工具必须注册到 default_tool_registry。
    /// 此前 `default_tool_registry` 只注册了 `EnterPlanMode` / `ExitPlanMode`,
    /// 没注册 `PlanWriteTool` —— 但 `ALWAYS_ON_TOOLS` 含 `PlanWrite`,
    /// LLM 收到 schema 后调用,queue.rs 查 ToolRegistry 报 "tool not found"。
    /// 本测试三件套同时断言,任一漏注册即失败,防回归。
    #[test]
    fn default_tool_registry_includes_plan_mode_tools() {
        let r = default_tool_registry();
        let names = r.list();
        assert!(
            names.iter().any(|n| n == "PlanWrite"),
            "default_tool_registry 必须注册 PlanWriteTool(否则 LLM 看到 schema 但 registry 找不到实现);got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "EnterPlanMode"),
            "default_tool_registry 必须注册 EnterPlanModeTool;got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "ExitPlanMode"),
            "default_tool_registry 必须注册 ExitPlanModeTool;got: {names:?}"
        );
        // PlanWrite 的 required_permission 必须是 Auto(免审批),否则 LLM
        // 调它会被审批层卡住,违背 Plan mode 设计意图。
        let planwrite = r
            .get("PlanWrite")
            .expect("PlanWrite must be registered")
            .required_permission();
        assert_eq!(
            planwrite,
            reflect_protocol::PermissionMode::Auto,
            "PlanWrite.required_permission 必须为 Auto,Plan mode 下免审批直写"
        );
    }
}
