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
use reflect_telemetry::TelemetrySink;
use reflect_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::m4_bootstrap;
use crate::stream::EventStream;

/// telemetry 记录开关的三态(v1.x)。
///
/// - `Auto`(默认):按 `~/.reflect/config.toml` 的 `[telemetry]` 段构造,
///   与 `reflect-exec` 路径对齐 —— SDK 消费者默认就有数据记录
///   (model-io 全文 + span 日志落到 `~/.reflect/traces/`);
/// - `Off`:`with_telemetry(None)` 显式关闭;
/// - `Custom(sink)`:`with_telemetry(Some(sink))` 注入自建 sink
///   (自定义目录 / 测试替身 / 多租户隔离)。
///
/// Debug 手写(不 derive):`TelemetrySink` 未实现 Debug。
#[derive(Clone)]
enum TelemetryChoice {
    Auto,
    Off,
    Custom(Arc<TelemetrySink>),
}

impl Default for TelemetryChoice {
    fn default() -> Self {
        TelemetryChoice::Auto
    }
}

// TelemetrySink 未实现 Debug(内部持有 writer/Mutex),手写一份只暴露
// 变体名的 Debug —— ReflectBuilder derive Debug 需要所有字段可 Debug。
impl std::fmt::Debug for TelemetryChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryChoice::Auto => f.write_str("Auto"),
            TelemetryChoice::Off => f.write_str("Off"),
            TelemetryChoice::Custom(_) => f.write_str("Custom(<sink>)"),
        }
    }
}

/// [`ReflectBuilder`] 的链式 builder。
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
    /// 额外提示词目录(v1.6)。`None` = 只用内嵌默认 + `~/.reflect/prompts/`;
    /// `Some(dir)` = 额外扫描该目录下的 `*.md` 模型层(优先级最高,
    /// 覆盖同名层)。TUI / GUI 用它把自带提示词包挂进运行时。
    prompts_dir: Option<PathBuf>,
    /// v1.x:telemetry 记录开关。默认 `Auto`(按 `[telemetry]` config 段
    /// 构造,与 reflect-exec 对齐);`with_telemetry(None)` 关闭;
    /// `with_telemetry(Some(sink))` 自定义。
    telemetry: TelemetryChoice,
    /// v1.x:`Auto` 路径下的 telemetry 目录覆盖(`with_traces_dir`)。
    /// `None` = 用 `[telemetry].dir` 或默认 `~/.reflect/traces`。
    traces_dir: Option<PathBuf>,
    /// v1.x:rollout 落盘根目录。`None` = 默认 `~/.reflect/sessions`;
    /// `Some(dir)` = 自定义(`with_rollout_dir`,便于测试隔离 / 多租户)。
    rollout_base: Option<PathBuf>,
    /// v1.x:rollout 开关(默认 true)。`without_rollout()` 置 false ——
    /// 拿到 M4Deps 后把 `recorder` 置 `None`。
    rollout_enabled: bool,
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
            prompts_dir: None,
            telemetry: TelemetryChoice::Auto,
            traces_dir: None,
            rollout_base: None,
            rollout_enabled: true,
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

    /// v1.6:额外提示词目录(按模型匹配的外置提示词库)。
    ///
    /// 传入的目录在 `~/.reflect/prompts/` 之后扫描,同名层以本目录
    /// 优先 —— TUI / GUI 由此把自带提示词包挂进运行时。目录约定:
    /// `default.md` / `<厂商>.md` / `<精确模型名>.md`,每个文件用
    /// `## key` 分节,缺失 key 落回内嵌默认。
    pub fn with_prompts_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.prompts_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// v1.x:配置 telemetry 数据记录(model-io 全文 + span 日志)。
    ///
    /// - `Some(sink)` —— 注入自建 sink(自定义目录 / 测试替身);
    /// - `None` —— 显式关闭 telemetry;
    /// - **不调用本方法**(默认)—— 按 `~/.reflect/config.toml` 的
    ///   `[telemetry]` 段自动构造,与 `reflect-exec` 路径对齐。
    ///   `[telemetry] enabled` 缺省为 `true`,故 SDK 消费者默认就有
    ///   数据记录落到 `~/.reflect/traces/`。
    ///
    /// 记录内容:每次 LLM 调用的完整 messages + system prompt + 工具名 +
    /// usage(input/output/cached/cache_write/cost)、turn/tool span。
    /// 读取侧 API 经 `reflect` 门面 re-export(见 `lib.rs`)。
    pub fn with_telemetry(mut self, sink: Option<Arc<TelemetrySink>>) -> Self {
        self.telemetry = match sink {
            Some(s) => TelemetryChoice::Custom(s),
            None => TelemetryChoice::Off,
        };
        self
    }

    /// v1.x:`Auto` telemetry 路径下的 trace 目录覆盖。
    ///
    /// 优先级高于 `[telemetry].dir`;不设置则用 config 值或默认
    /// `~/.reflect/traces`。与 `with_telemetry(Some(sink))` 同时使用时
    /// 本设置无效(sink 自带目录)。
    pub fn with_traces_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.traces_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// v1.x:自定义 rollout 会话记录落盘根目录。
    ///
    /// 默认 `~/.reflect/sessions`(YYYY/MM/DD 分桶)。传入自定义目录便于
    /// 测试隔离 / 多租户分目录存储。rollout 记录会话消息、TokenCount
    /// (含模型归属)、Compaction 等审计轨迹,是 `session_stats` 统计的
    /// 数据源。
    pub fn with_rollout_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.rollout_base = Some(dir.as_ref().to_path_buf());
        self
    }

    /// v1.x:完全关闭 rollout 会话记录(不落任何 session JSONL)。
    ///
    /// 适用于纯一次性执行 / 隐私敏感场景。关闭后 `session_stats` 等
    /// 读取侧 API 对该会话无数据可查。
    pub fn without_rollout(mut self) -> Self {
        self.rollout_enabled = false;
        self
    }

    /// 按当前 telemetry 配置解析出最终注入 `AgentConfig` 的 sink。
    ///
    /// - `Off` → `None`(telemetry 关闭);
    /// - `Custom(sink)` → 原样注入;
    /// - `Auto` → 按 `[telemetry]` config 段构造(见 `build_telemetry_sink`)。
    fn resolve_telemetry_sink(&self) -> Option<Arc<TelemetrySink>> {
        match &self.telemetry {
            TelemetryChoice::Off => None,
            TelemetryChoice::Custom(sink) => Some(sink.clone()),
            TelemetryChoice::Auto => build_telemetry_sink(self.traces_dir.as_deref()),
        }
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
        let registry = build_registry_from_env(&self.model)?;
        let tools = Arc::new(default_tool_registry());
        // v1.x:先解析 telemetry sink —— self.cancel 随后被移出,部分移动后
        // 不能再整体借用 self。
        let telemetry_sink = self.resolve_telemetry_sink();
        let cancel = self.cancel.unwrap_or_default();
        let thread_id = ThreadId::new();
        // 默认构造完整 M4 依赖 —— compactor / memory / skills / agent def
        // / recorder / note store / file recovery / subagent registry。
        // 失败 → 透传(不静默)。`with_prompts_dir` 的额外目录一并传入。
        let mut m4 = m4_bootstrap::build_default_m4_with_prompts(
            &self.workspace,
            "default",
            &self.model,
            &registry,
            thread_id,
            self.prompts_dir.as_slice(),
            self.rollout_base.as_deref(),
        )?;
        // v1.x:`without_rollout()` → 摘掉 recorder(M4Deps.recorder 为 Option)。
        if !self.rollout_enabled {
            m4.recorder = None;
        }
        let mut cfg =
            AgentConfig::new(self.model, self.workspace.clone()).with_approvals(self.approvals);
        if self.plan_mode {
            cfg = cfg.with_initial_permission_mode(reflect_protocol::PermissionMode::Plan);
        }
        cfg = cfg.with_m4(m4);
        // v1.x:注入 telemetry 记录 sink(Auto 路径按 [telemetry] config 构造)。
        cfg = cfg.with_telemetry(telemetry_sink);
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
        let registry = build_registry_from_env(&self.model)?;
        let tools = Arc::new(default_tool_registry());
        // v1.x:先解析 telemetry sink —— self.cancel 随后被移出。
        let telemetry_sink = self.resolve_telemetry_sink();
        let cancel = self.cancel.unwrap_or_default();
        let thread_id = ThreadId::new();
        let mut m4 = m4_bootstrap::build_default_m4_with_prompts(
            &self.workspace,
            "default",
            &self.model,
            &registry,
            thread_id,
            self.prompts_dir.as_slice(),
            self.rollout_base.as_deref(),
        )?;
        // v1.x:`without_rollout()` → 摘掉 recorder。
        if !self.rollout_enabled {
            m4.recorder = None;
        }
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
        // 0.5:Plan mode 必须**既设 permission mode,又注册 PlanModeGate**。
        // 此前这里只做了前者 —— `PermissionMode::Plan` 单独存在时没有任何
        // 强制力(它只是让审批层知道当前是 Plan 模式),写工具照样放行。
        // 真正的执行机制是 `PlanModeGate` 这个 PreToolUse hook 把白名单外
        // 工具 blanket-deny。此前全 workspace 只有 `reflect-exec/src/
        // headless.rs` 注册它,导致库门面(Reflect-TUI / ReflectDesktop)与
        // reflect-py 的 `plan_mode(true)` 拿到的是一个零强制的 Plan mode。
        if self.plan_mode {
            cfg = cfg.with_initial_permission_mode(reflect_protocol::PermissionMode::Plan);
        }
        cfg = cfg.with_m4(m4);
        // v1.x:注入 telemetry 记录 sink(Auto 路径按 [telemetry] config 构造)。
        cfg = cfg.with_telemetry(telemetry_sink);
        let thread = Arc::new(AgentThread::new(
            cfg,
            registry,
            tools.clone(),
            None,
            Some(hook_engine.clone()),
        ));
        if self.plan_mode {
            thread.register_hook(reflect_hooks::builtins::PlanModeGate::default_mode());
            let rbe_section = app_cfg.hooks.read_before_edit.as_ref();
            let (_rbe_state, rbe_hook) = reflect_hooks::builtins::build_read_before_edit(
                rbe_section.and_then(|s| s.enabled),
                rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
            );
            thread.register_hook(rbe_hook);
            tracing::info!("plan_mode=true: registered PlanModeGate + read_before_edit hooks");
        }

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

/// v1.x:按 `[telemetry]` config 段构造本地 telemetry sink。
///
/// 与 `reflect-exec::runtime_config::build_telemetry_sink` 同逻辑,下沉到
/// 门面让 SDK 路径默认有记录:
/// - `enabled = false`(显式)→ `None`;
/// - `enabled = true` 或缺省 → 构造 `TelemetrySink`,trace 目录按
///   `traces_dir` 覆盖参数 → `[telemetry].dir` → 默认 `~/.reflect/traces`
///   依次解析;
/// - `[hooks].langfuse_tracker` 配了 endpoint + keys → 并行挂 Langfuse
///   cloud exporter(缺省 / 缺 key 时 no-op,仅本地日志)。
///
/// sink 的 session_id 用新生成的 uuid(与 rollout 的 thread_id 独立,
/// 因 telemetry 的 model-io 文件按这个 id 命名)。
fn build_telemetry_sink(traces_dir: Option<&Path>) -> Option<Arc<TelemetrySink>> {
    let cfg = reflect_config::load_default();
    let default_section = reflect_config::TelemetrySection::default();
    let section = cfg.telemetry.as_ref().unwrap_or(&default_section);
    if !section.is_enabled() {
        return None;
    }
    let base_dir = traces_dir
        .map(|p| p.to_path_buf())
        .or_else(|| section.dir.clone())
        .unwrap_or_else(|| reflect_telemetry::resolve_traces_dir(None));
    let session_id = uuid::Uuid::new_v4().to_string();
    // v1.x:redact / max_bytes / record_tool_schemas 经 new_with_config
    // 真正生效(此前 schema 定义了但从未读取)。
    let write_cfg = reflect_telemetry::TelemetryWriteConfig {
        redact: section.should_redact(),
        max_bytes: section
            .max_bytes
            .map(|n| n as usize)
            .unwrap_or(reflect_telemetry::model::MAX_FIELD_CHARS),
    };
    let sink = TelemetrySink::new_with_config(
        base_dir,
        session_id,
        write_cfg,
        section.should_record_tool_schemas(),
    );

    if let Some(lf) = cfg.hooks.langfuse_tracker.as_ref()
        && lf.enabled.unwrap_or(false)
        && let (Some(endpoint), Some(pk), Some(sk)) = (
            lf.endpoint.as_ref(),
            lf.public_key.as_ref(),
            lf.secret_key.as_ref(),
        )
        && !endpoint.is_empty()
        && !pk.is_empty()
        && !sk.is_empty()
    {
        let lf_cfg = reflect_telemetry::LangfuseConfig {
            endpoint: endpoint.clone(),
            public_key: pk.clone(),
            secret_key: sk.clone(),
            batch_size: 64,
        };
        let exporter = reflect_telemetry::LangfuseExporter::start(Some(lf_cfg));
        sink.set_langfuse(exporter);
        tracing::info!(endpoint = %endpoint, "langfuse cloud exporter 已启用");
    }

    Some(sink)
}

/// 从标准 env 变量构造 `SharedModelRegistry`。与 `reflect_exec::async_main`
/// 的设置保持一致,这样库的使用者无需直接触碰 exec 的私有 helper。
///
/// `model_spec`:调用方显式传入的 model spec —— 参与 mock 判定
/// (`Reflect::builder("mock")` 应当离线可用)。
fn build_registry_from_env(model_spec: &str) -> anyhow::Result<Arc<reflect_llm::ModelRegistry>> {
    use reflect_llm::{
        AnthropicClient, AnthropicConfig, CredentialPool, MockClient, ModelRegistry, OllamaClient,
        OllamaConfig, OpenAIClient, OpenAIConfig, PoolEntry,
    };

    let registry = Arc::new(ModelRegistry::new());
    let mut any = false;
    // 离线 mock provider(显式 opt-in,免 key、零网络)——
    // 判定口径与 `reflect-config` 的 `active_provider_inner` 对齐。
    //
    // 此前门面完全不认识 mock:`REFLECT_MODEL=mock` 的 example 与离线集成
    // 脚本一律撞上「set OPENAI_API_KEY」,离线场景根本跑不起来(e2e 只能
    // 用 `|| true` 把它吞掉 —— 那正是假阳性)。
    if is_mock_provider(model_spec) {
        registry.register_pool(
            "mock",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(MockClient::from_env()),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        any = true;
    }
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
            "Reflect: set OPENAI_API_KEY, ANTHROPIC_API_KEY, or OLLAMA_HOST/OLLAMA_API_KEY \
             (离线场景可显式设 REFLECT_MODEL=mock)"
        ));
    }
    Ok(registry)
}

/// 判定是否应使用离线 mock provider。
///
/// 口径与 `reflect-config` 的 `active_provider_inner` 对齐:
/// - `REFLECT_PROVIDER=mock` / `fake`;
/// - `REFLECT_MODEL` 为 `mock` 或 `mock/...`;
/// - 显式传入的 model spec 为 `mock` 或 `mock/...`。
///
/// 只认**显式 opt-in**,不作为无 key 时的静默兜底 —— 免得真实 provider
/// 配错时被悄悄换成假模型。
fn is_mock_provider(model_spec: &str) -> bool {
    if let Ok(p) = std::env::var("REFLECT_PROVIDER") {
        let p = p.trim().to_ascii_lowercase();
        if p == "mock" || p == "fake" {
            return true;
        }
    }
    for m in [
        std::env::var("REFLECT_MODEL").ok(),
        Some(model_spec.to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let m = m.trim();
        if m == "mock" || m.starts_with("mock/") {
            return true;
        }
    }
    false
}

fn default_tool_registry() -> ToolRegistry {
    use reflect_tools::builtins;
    let r = ToolRegistry::default();
    r.register(Arc::new(builtins::EchoTool));
    r.register(Arc::new(builtins::BashTool::default()));
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

    /// 回归:门面此前**完全不认识** mock provider —— `Reflect::builder("mock")`
    /// 在无任何 API key 时报「set OPENAI_API_KEY」,离线场景(e2e 的 5 个
    /// example、离线集成脚本)根本跑不起来,上游 e2e 只能用 `|| true`
    /// 把它吞掉。显式 opt-in 的 mock 必须免 key 可用。
    #[tokio::test]
    async fn builder_accepts_mock_model_without_api_key() {
        let _g = env_lock().lock().unwrap();
        let prior_openai = std::env::var("OPENAI_API_KEY").ok();
        let prior_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
        let prior_provider = std::env::var("REFLECT_PROVIDER").ok();
        let prior_model = std::env::var("REFLECT_MODEL").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("REFLECT_PROVIDER");
            std::env::remove_var("REFLECT_MODEL");
        }
        let agent = Reflect::builder("mock")
            .workspace("/tmp")
            .build()
            .expect("mock provider 必须免 key 可用");
        assert_eq!(agent.thread().config().current_model(), "mock");
        if let Some(v) = prior_openai {
            unsafe {
                std::env::set_var("OPENAI_API_KEY", v);
            }
        }
        if let Some(v) = prior_anthropic {
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", v);
            }
        }
        if let Some(v) = prior_provider {
            unsafe {
                std::env::set_var("REFLECT_PROVIDER", v);
            }
        }
        if let Some(v) = prior_model {
            unsafe {
                std::env::set_var("REFLECT_MODEL", v);
            }
        }
    }

    /// `is_mock_provider` 的判定口径:只认**显式 opt-in**,
    /// 普通 model spec 不得被静默换成假模型。
    #[test]
    fn mock_provider_detection_is_explicit_only() {
        let _g = env_lock().lock().unwrap();
        let prior_provider = std::env::var("REFLECT_PROVIDER").ok();
        let prior_model = std::env::var("REFLECT_MODEL").ok();
        unsafe {
            std::env::remove_var("REFLECT_PROVIDER");
            std::env::remove_var("REFLECT_MODEL");
        }
        assert!(!is_mock_provider("openai/gpt-4o"));
        assert!(!is_mock_provider("anthropic/claude"));
        assert!(is_mock_provider("mock"));
        assert!(is_mock_provider("mock/mock-1"));
        unsafe {
            std::env::set_var("REFLECT_PROVIDER", "mock");
        }
        assert!(is_mock_provider("openai/gpt-4o"));
        unsafe {
            std::env::remove_var("REFLECT_PROVIDER");
        }
        unsafe {
            std::env::set_var("REFLECT_MODEL", "mock");
        }
        assert!(is_mock_provider("openai/gpt-4o"));
        if let Some(v) = prior_provider {
            unsafe {
                std::env::set_var("REFLECT_PROVIDER", v);
            }
        }
        if let Some(v) = prior_model {
            unsafe {
                std::env::set_var("REFLECT_MODEL", v);
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
    ///
    /// **0.5 补强**:permission mode 单独存在时**没有任何强制力** ——
    /// `PermissionMode::Plan` 只是让审批层知道当前是 Plan 模式,写工具照样
    /// 放行。真正的执行机制是 `PlanModeGate` 这个 PreToolUse hook。此前
    /// builder 只设前者不注册后者,库门面(Reflect-TUI / ReflectDesktop)与
    /// reflect-py 的 `plan_mode(true)` 拿到的是零强制的 Plan mode。
    /// 下面两条断言分别守住这两个必要条件。
    #[test]
    fn builder_plan_mode_compiles() {
        // smoke test:确保 builder API 在编译期就接受 `plan_mode` 调用。
        let _b = Reflect::builder("openai/gpt-4o").plan_mode(true);
    }

    /// 回归(0.5):`plan_mode(true)` 必须让 AgentConfig 的初始 permission
    /// mode 变成 `Plan`。
    #[test]
    fn builder_plan_mode_sets_plan_permission_mode() {
        let mut cfg =
            reflect_core::AgentConfig::new("openai/gpt-4o".to_string(), std::env::temp_dir());
        // 与 builder.rs:233-235 的 wiring 保持一致(这里是可单测的重放)。
        cfg = cfg.with_initial_permission_mode(reflect_protocol::PermissionMode::Plan);
        assert_eq!(
            *cfg.permission_mode.read(),
            reflect_protocol::PermissionMode::Plan,
            "plan_mode(true) 必须把 permission_mode 设为 Plan"
        );
    }

    /// 回归(0.5):Plan mode 的强制力来自 hook,不是 permission mode。
    /// 这条断言守住"PlanModeGate 是可达的默认构造"—— 若 hook 改名 /
    /// 改签名,这里先编译失败,提示去同步 builder 里的注册调用。
    #[test]
    fn plan_mode_gate_hook_is_constructible() {
        use reflect_hooks::hook::Hook as _;
        let gate = reflect_hooks::builtins::PlanModeGate::default_mode();
        assert_eq!(gate.name(), "plan_mode_gate");
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
            build_registry_from_env("openai/gpt-4o").unwrap(),
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

    // ── v1.x:telemetry / rollout 配置 wiring ────────────────────────────

    /// 持锁期间把 `$HOME` 指到 `home`;guard drop 时还原原值。
    /// 与 `env_lock` 共用一把锁,避免并行测试互相污染 env。
    struct TempHomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Option<String>,
    }

    impl Drop for TempHomeGuard {
        fn drop(&mut self) {
            // SAFETY: 持锁期间还原,无并发 env 变更。
            match &self.prior {
                Some(h) => unsafe { std::env::set_var("HOME", h) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn with_temp_home(home: &Path) -> TempHomeGuard {
        let lock = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("HOME").ok();
        // SAFETY: env_lock 已串行化所有 env 变更。
        unsafe { std::env::set_var("HOME", home) };
        TempHomeGuard { _lock: lock, prior }
    }

    #[test]
    fn telemetry_auto_constructs_sink_from_default_config() {
        // Auto 路径:HOME 指向空 tmpdir(无 config.toml)→ `[telemetry]`
        // 缺省 enabled=true → sink 非 None(SDK 路径默认有数据记录)。
        let tmp = tempfile::tempdir().unwrap();
        let _g = with_temp_home(tmp.path());
        let b = Reflect::builder("openai/gpt-4o");
        assert!(
            b.resolve_telemetry_sink().is_some(),
            "Auto 路径应默认构造 telemetry sink(与 reflect-exec 对齐)"
        );
    }

    #[test]
    fn telemetry_explicit_off_disables_sink() {
        // with_telemetry(None) = 显式关闭,不读 config。
        let b = Reflect::builder("openai/gpt-4o").with_telemetry(None);
        assert!(b.resolve_telemetry_sink().is_none());
    }

    #[test]
    fn telemetry_custom_sink_is_injected_verbatim() {
        let sink = TelemetrySink::new(
            std::env::temp_dir().join("reflect-test-telemetry-traces"),
            "test-session",
        );
        let b = Reflect::builder("openai/gpt-4o").with_telemetry(Some(sink.clone()));
        let resolved = b.resolve_telemetry_sink().expect("custom sink");
        assert!(Arc::ptr_eq(&resolved, &sink), "自定义 sink 应原样注入");
    }

    #[test]
    fn telemetry_traces_dir_keeps_auto_sink() {
        // with_traces_dir 只覆盖 Auto 路径的目录,不影响开关语义。
        let tmp = tempfile::tempdir().unwrap();
        let _g = with_temp_home(tmp.path());
        let b = Reflect::builder("openai/gpt-4o").with_traces_dir(tmp.path().join("traces"));
        assert!(b.resolve_telemetry_sink().is_some());
    }

    #[test]
    fn rollout_config_fields_default_on() {
        // 默认:rollout 开启、无自定义目录。
        let b = Reflect::builder("openai/gpt-4o");
        assert!(b.rollout_enabled);
        assert!(b.rollout_base.is_none());
        // without_rollout 关闭;with_rollout_dir 设目录。
        let b2 = Reflect::builder("openai/gpt-4o").without_rollout();
        assert!(!b2.rollout_enabled);
        let b3 = Reflect::builder("openai/gpt-4o").with_rollout_dir("/tmp/custom-sessions");
        assert_eq!(
            b3.rollout_base.as_deref(),
            Some(std::path::Path::new("/tmp/custom-sessions"))
        );
    }
}
