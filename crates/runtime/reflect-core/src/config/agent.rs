//! `AgentConfig` — `AgentThread` 的运行时配置。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use reflect_llm::{RoutingPolicy, SharedQuotaTracker};
use reflect_protocol::{AbortReason, PermissionMode, ReasoningEffortMirror, ThreadId};
use reflect_tools::SessionWorktreeState;
use tokio_util::sync::CancellationToken;

use super::env::DEFAULT_MAX_ITERATIONS;
use super::m4::M4Deps;

/// `AgentThread` 的运行时配置。
#[derive(Clone)]
pub struct AgentConfig {
    /// 默认 model spec(如 `"openai/gpt-4o"`、`"anthropic/claude-3-5-sonnet-latest"`)。
    ///
    /// v0.2.2 起改为 `Arc<RwLock<String>>`:`reflect-exec` 的热重载 task 会在
    /// `~/.reflect/config.toml` 变更时调 `set_model()` 写入新值,后续 turn
    /// 通过 [`Self::current_model`] 读到最新 spec。`Clone` 走 `Arc`,所有
    /// 副本共享同一把锁 —— 见 `submission_loop` 把 `cfg` 移到 `AgentThread`
    /// 之后多处持有 `cfg.clone()` 的语义不变。
    pub model: Arc<RwLock<String>>,
    /// v1.x Plan mode:会话级 `PermissionMode` 状态机。
    ///
    /// 同样用 `Arc<RwLock<…>>` 镜像 `model` 的热重载 pattern:`/plan`
    /// slash 或 `EnterPlanModeTool` 触发后由 `submission_loop` 调
    /// [`Self::set_permission_mode`] 写入新值,所有 hook 引擎(包括
    /// `PlanModeGate`)和 tool queue 通过 [`Self::permission_mode`]
    /// 读到最新 mode。默认 `PermissionMode::Auto`(普通执行模式)。
    pub permission_mode: Arc<RwLock<PermissionMode>>,
    /// 工作区根路径 —— 各工具相对该路径操作。
    ///
    /// v1.x Git worktree:`EnterWorktreeTool` / `ExitWorktreeTool` 通过
    /// [`Self::set_workspace`] 热切换;`ToolContext.workspace` 持有同一把
    /// `RwLock`,后续 tool 调用立刻读到新路径。
    pub workspace: Arc<RwLock<PathBuf>>,
    /// 当前 worktree 隔离会话;`None` 表示未进入 worktree。
    pub worktree: Arc<RwLock<Option<SessionWorktreeState>>>,
    /// 协作式取消令牌(Ctrl-C、`Op::Interrupt` 等)。
    pub cancel: CancellationToken,
    /// M4 依赖。测试中全为 `None`;`reflect-exec` 在启动时填充。
    /// 为 `None` 时 submission loop 回退到 no-op / 空实现,
    /// 保证既有测试继续可用。
    pub m4: Option<M4Deps>,
    /// M6:安装回合级 `ApprovalGate`,让 `Prompt` 权限工具走
    /// `EventMsg::ApprovalRequest`。headless `reflect-exec` 的 JSONL
    /// 路径默认 `false`;TUI / lib facade 构造线程时打开。
    /// 高级用法还可通过 `REFLECT_APPROVALS=1` 环境变量启用。
    pub approvals: bool,
    /// v1.0 多 Provider 路由:角色 → spec slot 的路由策略。
    ///
    /// `model_call` 入口用 `policy.resolve(Role::Main)` 拿到 spec,失败
    /// 时由 `ModelRegistry::next_for` 在 pool 内自动切下一个 credential。
    /// 默认 `Arc::new(RoutingPolicy::default())`;`reflect-exec::bootstrap_m4`
    /// 在读到 `[routing]` 段后调 [`Self::set_policy`] 替换为实际策略。
    pub policy: Arc<RoutingPolicy>,
    /// v1.x S4:`/effort low|medium|high` 透传槽。`model_call` 入口读
    /// 当前值构造 `ChatRequest::thinking = ThinkingConfig::OpenAIReasoning
    /// { effort: llm_effort(mirror) }`,下一轮 LLM 调用立刻生效。
    ///
    /// 与 `permission_mode` 同 pattern:`submission_loop` 在收到
    /// `Op::SetEffort` 后调 [`Self::set_effort`] 写入;`Clone` 后多副本
    /// 共享同一把 `RwLock`。默认 `Low`,与 Anthropic / OpenAI 默认
    /// reasoning 强度一致(避免 "未设置 = 不思考" 的歧义)。
    pub effort: Arc<RwLock<ReasoningEffortMirror>>,
    /// S5a:可选 permission resolver,注入 `ApprovalGate::with_state`。
    /// TUI / reflect-exec 从 `FilePermissionStore` 构造;测试 / headless
    /// 默认 `None`(modal-only 行为)。
    pub permission_resolver: Option<Arc<dyn reflect_permissions::PermissionResolver>>,
    /// P2 `yolo-classifier`:Auto 模式下的启发式审批分类器,注入
    /// `ApprovalGate`(经 `submission_loop` per-turn 设置)。`None` = Auto
    /// 模式落到 modal(向后兼容)。
    pub yolo_classifier: Option<Arc<dyn reflect_permissions::YoloClassifier>>,
    /// YOLO 自动批准的置信度阈值(>= 此值才信任 Allow 建议)。默认 0.8。
    pub yolo_threshold: f32,
    /// v1.2 P1-12:会话级累计 token 用量(跨所有 turn 累加,镜像 `model` /
    /// `effort` 的 `Arc<RwLock<…>>` 共享模式)。`model_call` 每次调用后
    /// 累加 `_usage`;`get_context_remaining` 工具 / 预算检查都读这把锁。
    /// 与 `AgentState.total_usage`(单 turn 累加、每 turn reset)正交。
    pub session_usage: Arc<RwLock<reflect_protocol::TokenUsage>>,
    /// v1.2 P1-12:会话级 token 预算硬上限。`None` = 仅靠 `max_iterations`;
    /// `Some(n)` = `session_usage.total_tokens >= n` 时终止当前 turn
    /// (`TurnStatus::TokenBudgetExceeded`)。`reflect-exec` 启动期从
    /// `[token_budget].session_total_tokens` / env 解析填入。用 `Arc<RwLock>`
    /// 包裹(同 `model` / `effort` pattern),让 `handle_reload` 热重载后
    /// 下一轮 `model_call` 通过共享句柄读到新值。
    pub token_budget: Arc<RwLock<Option<u64>>>,
    /// v1.x:agent 主循环全局迭代上限(`model_call` 进入次数)。默认
    /// [`DEFAULT_MAX_ITERATIONS`] (32),与历史硬编码值一致。`reflect-exec`
    /// 启动期从 env `REFLECT_MAX_ITERATIONS` / TOML
    /// `active.max_iterations` 解析填入。用 `Arc<RwLock>` 包裹(同
    /// `model` / `token_budget` pattern),让 `handle_reload` 热重载后下一轮
    /// `submission_loop` 通过共享句柄读到新值。可被 `AgentDefinition.max_turns`
    /// / `SubAgentSpec.max_turns` 进一步收紧(取 `min`)。
    pub max_iterations: Arc<RwLock<u32>>,
    /// v1.2 P1-12:当前 model 的上下文窗口大小(token)。引擎在
    /// `SessionConfigured` 时用 `reflect_llm::context_window_for` 算出后写入
    /// (热重载切 model 后刷新)。`get_context_remaining` 工具读它做分母。
    pub context_window_size: Arc<RwLock<Option<u32>>>,
    /// Per-model 上下文窗口覆盖表(从 config.toml `[context_windows]` 段读入,
    /// 优先于内置 `context_window_for` 静态回退表,让私端/新模型无需改代码)。
    /// submission_loop 在 `SessionConfigured` 时先查这个 map,再走回退表。
    pub context_window_overrides: Arc<RwLock<HashMap<String, u32>>>,
    /// v1.2 P1-12(已有-B):`/compact` 手动触发标志。`Op::Compact` 设置
    /// `true`,下一个 turn 的 `pre_loop` 据此强制运行 compactor(无视
    /// trigger_tokens 阈值),压缩后清零。共享 `Arc<RwLock>` 让
    /// `submission_loop` 的 `Op::Compact` 分支与 `pre_loop` 节点通信。
    pub force_compact_next: Arc<RwLock<bool>>,
    /// v1.2 P1:本地 Langfuse 式日志 sink。由 `reflect-exec` /
    /// `reflect-tui` 在 bootstrap 时按 `[telemetry]` 配置构造。
    /// `None` = telemetry 关闭(测试 / 旧调用方)。
    pub telemetry: Option<Arc<reflect_telemetry::TelemetrySink>>,
    /// v1.2 P1:目标模式 controller(参考 zcode `/goal` 设计)。
    /// `None` = 目标模式未激活;`/goal <obj>` 时由 submission_loop 构造。
    /// turn 结束后 `on_turn_end` 自校验,未完成则推 steering 续作。
    pub goal: Option<Arc<reflect_goal::GoalController>>,
    /// v1.x 功能 7:token plan 配额追踪器。`None` = 无 credential 声明配额
    /// (向后兼容);`Some` = `model_call` 累计 usage,耗尽触发 cooldown 切换。
    pub quota_tracker: Option<SharedQuotaTracker>,
    /// 工具级环境变量(从 `config.toml` 的 `[web_search]` 等段注入)。
    /// `shared_tool_context()` 把它 clone 到每个 `ToolContext.env`,
    /// 让 `web_search` 等工具读到 `BRAVE_API_KEY` 等密钥。缺省空 map。
    pub tool_env: HashMap<String, String>,
    /// v1.x:可选的 session id。`None`(默认)= `submission_loop` 内部
    /// `ThreadId::new()` 自行分配(历史行为);`Some(id)` = 外部预分配,
    /// 让 recorder 文件名、`SessionMeta.session_id`、`SessionConfigured`
    /// 报告的 id 三者一致。TUI / 需要持久化的调用方在构造 recorder 前
    /// 预分配 id 并通过 [`Self::with_session_id`] 注入,避免"文件名 id
    /// 与 JSONL 内 SessionMeta id 不一致"的漂移。
    pub session_id: Option<ThreadId>,
    /// v1.x resume:`--resume` / `-c` 续作时,从历史 JSONL 回放出的
    /// 先前对话消息。submission_loop 在**首个 turn** 把这些消息**前置**
    /// 到当前用户输入之前,让 `pre_loop` 把它们 seed 进
    /// `state.messages`,恢复的 agent 因此记得历史上下文。
    ///
    /// 此前 resume 分支只用了 `bundle.initial_messages.len()` 拼一条
    /// system-reminder,真正的历史被丢弃 —— 恢复后的 agent 毫无记忆。
    /// 普通路径(non-resume)留空,行为不变。
    ///
    /// 关键:只在首个 turn 消费一次。`Arc<RwLock<Vec>>` 让
    /// submission_loop 消费后清空,避免后续跨提交 turn 再次前置造成
    /// 历史重复(`[old, resume, reply, old, new]` —— 历史 echo)。
    pub preload_messages: Arc<RwLock<Vec<reflect_llm::ChatMessage>>>,
    /// v1.x Plan mode:`submission_loop` 在 `Op::Interrupt` / 自然 turn
    /// 失败路径写入最近一次 abort 原因,下一次 turn 的 `pre_loop` 用
    /// `take_last_abort_reason()` 一次性消费后注入 ephemeral system
    /// block,提醒 LLM「上轮被中断,在 Plan mode 下应当收尾并调
    /// `ExitPlanMode`」。
    ///
    /// 走 take-once 语义(`take` 而非 `get`)保证:
    /// 1. 中断信号只对「紧邻的下一个 turn」生效,不会被多 turn 重复消费
    ///    造成持续唠叨。
    /// 2. 普通 turn 失败(`TurnAborted` 但不是用户主动中断)同样会被记
    ///    录 —— `pre_loop` 自己决定是否要在 Plan mode 下提示。
    ///
    /// 默认 `None`;`Clone` 共享同一把 `RwLock`,与 `permission_mode`
    /// 镜像。
    pub last_abort_reason: Arc<RwLock<Option<AbortReason>>>,
    /// v1.4 A1:子代理运行注册表(在飞子代理的取消令牌表)。
    ///
    /// `None`(默认)= 本线程不追踪子代理(测试 / 纯库使用),
    /// `Op::Interrupt { child_id }` 定向分支在此情况下退化为 warn;
    /// `Some` = 注册表由调用方创建后同时注入本配置与 `SubAgentFactory`
    /// (两者共享同一 `Arc`),`spawn` 登记、`SpawnedChild` 终态注销。
    /// `Clone` 走 `Arc`,所有副本共享同一张表。
    pub subagent_runtime: Option<Arc<crate::subagent_registry::SubagentRuntimeRegistry>>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self::new("", "")
    }
}

impl AgentConfig {
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: Arc::new(RwLock::new(model.into())),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Auto)),
            workspace: Arc::new(RwLock::new(workspace.into())),
            worktree: Arc::new(RwLock::new(None)),
            cancel: CancellationToken::new(),
            m4: None,
            approvals: false,
            policy: Arc::new(RoutingPolicy::default()),
            effort: Arc::new(RwLock::new(ReasoningEffortMirror::Low)),
            permission_resolver: None,
            yolo_classifier: None,
            yolo_threshold: 0.8,
            session_usage: Arc::new(RwLock::new(reflect_protocol::TokenUsage::default())),
            token_budget: Arc::new(RwLock::new(None)),
            max_iterations: Arc::new(RwLock::new(DEFAULT_MAX_ITERATIONS)),
            context_window_size: Arc::new(RwLock::new(None)),
            context_window_overrides: Arc::new(RwLock::new(HashMap::new())),
            force_compact_next: Arc::new(RwLock::new(false)),
            telemetry: None,
            goal: None,
            quota_tracker: None,
            tool_env: HashMap::new(),
            session_id: None,
            preload_messages: Arc::new(RwLock::new(Vec::new())),
            last_abort_reason: Arc::new(RwLock::new(None)),
            subagent_runtime: None,
        }
    }

    /// 当前激活的 model spec 快照(读 `RwLock` 后 clone)。读侧唯一入口;
    /// `pre_loop` / `model_call` / 提交循环等都在这里取最新值,以便
    /// 热重载后下一个 turn 立刻用上新 model。
    pub fn current_model(&self) -> String {
        self.model.read().clone()
    }

    /// 写入新 model spec。仅供 `reflect-exec::handle_reload` 在 TOML 热重载
    /// 检测到 model 变更时调用 —— 库用户想在会话内切 model 也走这条
    /// `reflect-config` reload 路径,直接调用会绕过 diff / event 通知。
    pub fn set_model(&self, new_spec: impl Into<String>) {
        *self.model.write() = new_spec.into();
    }

    /// 当前会话的 `PermissionMode` 快照。
    ///
    /// 默认 `PermissionMode::Auto`(普通执行模式);TUI `/plan` slash 或
    /// `EnterPlanModeTool` 触发后由 `submission_loop` 切到 `Plan`,
    /// 期间 `PlanModeGate` hook 会 blanket-deny 写工具。
    pub fn permission_mode(&self) -> PermissionMode {
        *self.permission_mode.read()
    }

    /// 写入新 `PermissionMode`。仅供 `submission_loop` 在用户批准
    /// `EnterPlanModeTool` / `ExitPlanModeTool` 后调用;直接调用会
    /// 绕过 `EventMsg::PermissionModeChanged` 通知路径。
    pub fn set_permission_mode(&self, new_mode: PermissionMode) {
        *self.permission_mode.write() = new_mode;
    }

    /// v1.x Plan mode:写入最近一次 abort 原因。
    ///
    /// 仅供 `submission_loop` 在发出 `TurnAborted` 事件时调用(用户
    /// Esc / Stop,或 turn 因 token 预算 / 内部错误结束)。
    /// `pre_loop` 用 [`Self::take_last_abort_reason`] 一次性消费 —— 这
    /// 里只覆盖式写入,不清空。
    pub fn set_last_abort_reason(&self, reason: AbortReason) {
        *self.last_abort_reason.write() = Some(reason);
    }

    /// v1.x Plan mode:消费最近一次 abort 原因。
    ///
    /// `take` 语义:读后立即清空,保证只对下一个 turn 注入一次 ephemeral
    /// 提醒。若没有待消费的 abort,返回 `None`。
    pub fn take_last_abort_reason(&self) -> Option<AbortReason> {
        self.last_abort_reason.write().take()
    }

    /// v1.x S4:当前会话的 `ReasoningEffortMirror` 快照。
    ///
    /// 默认 `Low`;`submission_loop` 在收到 `Op::SetEffort` 后调
    /// [`Self::set_effort`] 写入新值,`model_call` 在构造 `ChatRequest`
    /// 时调 [`Self::current_effort`] 取最新值。
    pub fn current_effort(&self) -> ReasoningEffortMirror {
        *self.effort.read()
    }

    /// v1.x S4:写入新 reasoning effort。仅供 `submission_loop` 在收到
    /// `Op::SetEffort` 后调用;直接调用会绕过 `tracing::info!` 审计行。
    pub fn set_effort(&self, new_effort: ReasoningEffortMirror) {
        *self.effort.write() = new_effort;
    }

    /// 当前 workspace 根路径快照。
    pub fn current_workspace(&self) -> PathBuf {
        self.workspace.read().clone()
    }

    /// 热切换 workspace。`EnterWorktreeTool` / `ExitWorktreeTool` 在
    /// 用户审批通过后调用;与 `ToolContext` 共享 `Arc<RwLock<…>>`。
    pub fn set_workspace(&self, path: impl Into<PathBuf>) {
        *self.workspace.write() = path.into();
    }

    /// 读取 worktree 会话状态。
    pub fn worktree_state(&self) -> Option<SessionWorktreeState> {
        self.worktree.read().clone()
    }

    /// 写入 worktree 会话状态。
    pub fn set_worktree_state(&self, state: Option<SessionWorktreeState>) {
        *self.worktree.write() = state;
    }

    /// 构造 `ToolContext` 时共享 workspace / worktree / session_usage /
    /// token_budget 句柄。
    pub fn shared_tool_context(&self) -> reflect_tools::ToolContext {
        let mut ctx = reflect_tools::ToolContext::with_shared(
            Arc::clone(&self.workspace),
            Arc::clone(&self.worktree),
        );
        // v1.2 P1-12:共享会话用量 / 预算 / 上下文窗口句柄,让
        // `get_context_remaining` 工具读到 `model_call` 实时累加的值
        // (同 `workspace` 共享 pattern)。
        ctx.session_usage = Arc::clone(&self.session_usage);
        ctx.token_budget = Arc::clone(&self.token_budget);
        ctx.context_window_size = Arc::clone(&self.context_window_size);
        // 工具级 env(如 BRAVE_API_KEY)从 TOML 段注入到每个 ToolContext。
        ctx.env = self.tool_env.clone();
        ctx
    }

    /// 设置 M4 依赖。由 `reflect-exec` 启动时调用。
    pub fn with_m4(mut self, m4: M4Deps) -> Self {
        self.m4 = Some(m4);
        self
    }

    /// v1.x:注入预分配的 `ThreadId` 作为本会话 id(见 `session_id` 字段
    /// 文档)。调用方应先 `ThreadId::new()` 再把同一 id 传给 recorder 构造
    /// 与本方法,保证 `recorder 文件名 / SessionMeta.session_id /
    /// SessionConfigured.session_id` 三者一致。
    pub fn with_session_id(mut self, id: ThreadId) -> Self {
        self.session_id = Some(id);
        self
    }

    /// v1.x resume:把回放出的历史消息设为 preload 历史。submission_loop
    /// 会在首个 turn 把它们前置到当前用户输入之前,恢复的 agent 因此记得
    /// 之前的对话。普通路径不调用此方法(留空)。
    pub fn with_preload_messages(mut self, messages: Vec<reflect_llm::ChatMessage>) -> Self {
        self.preload_messages = Arc::new(RwLock::new(messages));
        self
    }

    /// 启用每 turn 的 `ApprovalGate`。TUI / lib 调用此方法;
    /// headless exec driver 保持关闭。
    pub fn with_approvals(mut self, on: bool) -> Self {
        self.approvals = on;
        self
    }

    /// v1.x Plan mode:在构造时设置初始 `PermissionMode`(用于 `--plan-mode` CLI 旗标)。
    pub fn with_initial_permission_mode(self, mode: PermissionMode) -> Self {
        *self.permission_mode.write() = mode;
        self
    }

    /// S5a:挂载 `StorePermissionResolver`(与 TUI `/permissions` 同源 store)。
    pub fn with_permission_resolver(
        mut self,
        resolver: Arc<dyn reflect_permissions::PermissionResolver>,
    ) -> Self {
        self.permission_resolver = Some(resolver);
        self
    }

    /// 用外部 `CancellationToken` 替换默认 token —— 集成测试共享 cancel
    /// 用。生产代码不需要这条路径 (`reflect-exec` 自己 wire Ctrl-C)。
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// v1.4 A1:注入子代理运行注册表。调用方(exec bootstrap)创建一个
    /// `Arc<SubagentRuntimeRegistry>` 后,同时传给本方法与
    /// `SubAgentFactory::set_runtime_registry`,让 `Op::Interrupt
    /// { child_id }` 能定向路由到在飞子代理。
    pub fn with_subagent_runtime(
        mut self,
        registry: Arc<crate::subagent_registry::SubagentRuntimeRegistry>,
    ) -> Self {
        self.subagent_runtime = Some(registry);
        self
    }

    /// 构造时一次性塞入 `RoutingPolicy`(给 `bootstrap_m4` 启动期用)。
    /// v1.0 Phase 1:`policy` 字段构造后不可变;热重载在 Phase 3 接
    /// `Arc<ArcSwap<RoutingPolicy>>` 后另开 `set_policy()` 方法。
    pub fn with_policy(mut self, policy: Arc<RoutingPolicy>) -> Self {
        self.policy = policy;
        self
    }

    /// v1.2 P1-12:构造时设置会话 token 预算上限。
    pub fn with_token_budget(self, budget: Option<u64>) -> Self {
        *self.token_budget.write() = budget;
        self
    }

    /// 构造时注入 per-model 上下文窗口覆盖表(从 config.toml
    /// `[context_windows]` 段读入)。`submission_loop` 在每次
    /// `SessionConfigured` 时先查这个 map(归一化后精确/前缀匹配),再走
    /// `reflect_llm::context_window_for` 静态回退表。
    pub fn with_context_window_overrides(self, overrides: HashMap<String, u32>) -> Self {
        *self.context_window_overrides.write() = overrides;
        self
    }

    /// v1.2 P1:注入本地 Langfuse 式日志 sink。由 `reflect-exec` /
    /// `reflect-tui` 在 bootstrap 时按 `[telemetry]` 配置构造后注入。
    /// `None` = telemetry 关闭。
    pub fn with_telemetry(mut self, sink: Option<Arc<reflect_telemetry::TelemetrySink>>) -> Self {
        self.telemetry = sink;
        self
    }

    /// v1.x 功能 7:注入 token plan 配额追踪器。由 `reflect-exec` bootstrap
    /// 在解析 config(收集所有声明了 `quota` 的 credential)后构造注入。
    /// `None` = 不追踪配额(无 credential 声明 / 测试)。
    pub fn with_quota_tracker(mut self, tracker: Option<SharedQuotaTracker>) -> Self {
        self.quota_tracker = tracker;
        self
    }

    /// 构造时注入工具级环境变量(从 `[web_search].api_key` 等 TOML 段
    /// 读取)。`shared_tool_context()` 会把这份 map clone 到每个
    /// `ToolContext.env`,让 `web_search` 读到 `BRAVE_API_KEY`。
    pub fn with_tool_env(mut self, env: HashMap<String, String>) -> Self {
        self.tool_env = env;
        self
    }

    /// v1.2 P1-12:热重载时更新会话 token 预算上限。`handle_reload`
    /// 在检测到 `[token_budget]` 段变更时调用;`Clone` 后多副本共享
    /// 同一把 RwLock,下一轮 `model_call` 读最新值(与 `set_model` /
    /// `set_effort` 同 pattern)。
    pub fn set_token_budget(&self, budget: Option<u64>) {
        *self.token_budget.write() = budget;
    }

    /// 热重载 `with_context_window_overrides` 副本:覆盖 `submission_loop`
    /// 下一次 `SessionConfigured` 时的优先表。后续切换 model 自动查最新值。
    pub fn set_context_window_overrides(&self, overrides: HashMap<String, u32>) {
        *self.context_window_overrides.write() = overrides;
    }

    /// 当前覆盖表快照,用于测试 / 诊断。
    pub fn current_context_window_overrides(&self) -> HashMap<String, u32> {
        self.context_window_overrides.read().clone()
    }

    /// v1.2 P1-12:会话 token 预算快照(`None` = 无上限)。
    pub fn current_token_budget(&self) -> Option<u64> {
        *self.token_budget.read()
    }

    /// v1.x:构造时设置 agent 主循环全局迭代上限。给 `bootstrap`
    /// 启动期用(`max_iterations_from_env` 解析的结果注入)。
    pub fn with_max_iterations(self, max_iterations: u32) -> Self {
        *self.max_iterations.write() = max_iterations;
        self
    }

    /// v1.x:热重载时更新全局迭代上限。`handle_reload` 在检测到
    /// `active.max_iterations` 段变更时调用;`Clone` 后多副本共享同一把
    /// RwLock,下一轮 `submission_loop` 读最新值(与 `set_token_budget`
    /// 同 pattern)。
    pub fn set_max_iterations(&self, max_iterations: u32) {
        *self.max_iterations.write() = max_iterations;
    }

    /// v1.x:全局迭代上限快照(`submission_loop` 构造 `NodeContext` 时读)。
    pub fn current_max_iterations(&self) -> u32 {
        *self.max_iterations.read()
    }

    /// v1.2 P1-12(已有-B):设置 `/compact` 强制标志(由 `Op::Compact` 调)。
    pub fn request_force_compact(&self) {
        *self.force_compact_next.write() = true;
    }

    /// v1.2 P1-12(已有-B):读 + 清零强制标志(由 `pre_loop` 调,返回是否该
    /// 强制压缩本轮)。
    pub fn take_force_compact(&self) -> bool {
        let mut g = self.force_compact_next.write();
        let v = *g;
        *g = false;
        v
    }

    /// v1.2 P1-12:会话级累计 token 用量快照(读 `RwLock` 后 clone)。
    /// `get_context_remaining` 工具与预算检查的读侧入口。
    pub fn current_session_usage(&self) -> reflect_protocol::TokenUsage {
        self.session_usage.read().clone()
    }

    /// v1.2 P1-12:把单次 model_call 的 `_usage` 累加进会话级总量。
    /// `model_call` 每次调用后调;`Clone` 后多副本共享同一把 RwLock,
    /// 与单 turn 的 `AgentState.total_usage`(每 turn reset)正交。
    pub fn add_session_usage(&self, delta: &reflect_protocol::TokenUsage) {
        let mut s = self.session_usage.write();
        s.input_tokens = s.input_tokens.saturating_add(delta.input_tokens);
        s.output_tokens = s.output_tokens.saturating_add(delta.output_tokens);
        s.cached_tokens = s.cached_tokens.saturating_add(delta.cached_tokens);
        s.cache_write_tokens = s
            .cache_write_tokens
            .saturating_add(delta.cache_write_tokens);
        s.total_tokens = s.total_tokens.saturating_add(delta.total_tokens);
    }

    /// v1.2 P1-12:会话预算是否已耗尽。`None` budget 永不耗尽。
    pub fn session_budget_exceeded(&self) -> bool {
        match *self.token_budget.read() {
            Some(limit) => self.session_usage.read().total_tokens as u64 >= limit,
            None => false,
        }
    }
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model", &self.current_model())
            .field("permission_mode", &self.permission_mode())
            .field("workspace", &self.current_workspace())
            .field("worktree", &self.worktree_state())
            .field("approvals", &self.approvals)
            .field(
                "permission_resolver",
                &self
                    .permission_resolver
                    .as_ref()
                    .map(|_| "<dyn PermissionResolver>"),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── v0.2.2 热重载切 model ──────────────────────────────────────────

    /// `current_model` 读初始值。
    #[test]
    fn agent_config_current_model_returns_initial_value() {
        let cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
        assert_eq!(cfg.current_model(), "anthropic/claude-3-5-sonnet-latest");
    }

    /// `set_model` 写入新值,`current_model` 立刻读到。
    #[test]
    fn agent_config_set_model_updates_current_model() {
        let cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
        cfg.set_model("openai/gpt-4o");
        assert_eq!(cfg.current_model(), "openai/gpt-4o");
    }

    /// `Clone` 后两副本共享同一把 RwLock —— 写其中一个,另一个也看见。
    /// 这是热重载能贯穿 `submission_loop` 多处 `cfg.clone()` 的基础。
    #[test]
    fn agent_config_clone_shares_rwlock() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_model("openai/gpt-4o");
        assert_eq!(
            cfg2.current_model(),
            "openai/gpt-4o",
            "clone must observe writes via shared RwLock"
        );
    }

    /// `Debug` 输出包含 model spec —— `ReflectBuilder` 的 Debug 用例和
    /// snapshot 测试都依赖这一行。
    #[test]
    fn agent_config_debug_includes_model_spec() {
        let cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");
        let dbg = format!("{cfg:?}");
        assert!(
            dbg.contains("anthropic/claude-3-5-sonnet-latest"),
            "Debug must include model spec, got: {dbg}"
        );
    }

    // ── v1.x Plan mode: permission_mode 热重载 ─────────────────────────────

    /// 默认 `permission_mode` 必须是 `Auto`(普通执行模式)。
    #[test]
    fn agent_config_permission_mode_defaults_to_auto() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert_eq!(cfg.permission_mode(), PermissionMode::Auto);
    }

    /// `set_permission_mode` 写入新值,`permission_mode()` 立即读到。
    #[test]
    fn agent_config_set_permission_mode_updates_current() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        cfg.set_permission_mode(PermissionMode::Plan);
        assert_eq!(cfg.permission_mode(), PermissionMode::Plan);
        cfg.set_permission_mode(PermissionMode::Prompt);
        assert_eq!(cfg.permission_mode(), PermissionMode::Prompt);
    }

    /// `Clone` 后两副本共享同一把 RwLock —— 镜像 model 的 pattern,
    /// 让 `submission_loop` 多处持有 `cfg.clone()` 后,Plan 模式切换对所有
    /// 副本都可见。
    #[test]
    fn agent_config_clone_shares_permission_mode_rwlock() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_permission_mode(PermissionMode::Plan);
        assert_eq!(
            cfg2.permission_mode(),
            PermissionMode::Plan,
            "clone 必须能观察到 permission_mode 写入"
        );
    }

    /// `with_initial_permission_mode` 构造器便捷设置(给 `--plan-mode` CLI 旗标用)。
    #[test]
    fn agent_config_with_initial_permission_mode() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp")
            .with_initial_permission_mode(PermissionMode::Plan);
        assert_eq!(cfg.permission_mode(), PermissionMode::Plan);
    }

    // ── v1.x Plan mode:last_abort_reason take-once 语义 ────────────────

    /// 默认 `last_abort_reason` 必须是 `None`(没有任何中断历史)。
    #[test]
    fn agent_config_last_abort_reason_defaults_to_none() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert_eq!(cfg.take_last_abort_reason(), None);
    }

    /// `set_last_abort_reason` 写入后,`take_last_abort_reason` 读到一次
    /// 后自动清空 —— 这是防止「下一轮 turn 之后再次被 pre_loop 消费、
    /// 持续唠叨上轮中断」的关键不变量。
    #[test]
    fn agent_config_take_last_abort_reason_consumes_once() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        cfg.set_last_abort_reason(AbortReason::UserInterrupt);
        // 第一次 take:读到值。
        assert_eq!(
            cfg.take_last_abort_reason(),
            Some(AbortReason::UserInterrupt)
        );
        // 第二次 take:已经清空,返回 None。
        assert_eq!(cfg.take_last_abort_reason(), None);
    }

    /// `Clone` 后两副本共享同一把 RwLock —— `submission_loop` 在一个
    /// 副本上调 `set_last_abort_reason`,另一个副本能观察到。
    #[test]
    fn agent_config_clone_shares_last_abort_reason_rwlock() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_last_abort_reason(AbortReason::UserInterrupt);
        assert_eq!(
            cfg2.take_last_abort_reason(),
            Some(AbortReason::UserInterrupt),
            "clone 必须能观察到 last_abort_reason 写入"
        );
    }

    // ── v1.x S4:effort 热重载 ──────────────────────────────────────

    /// 默认 effort = Low(Anthropic / OpenAI 默认 reasoning 强度)。
    #[test]
    fn agent_config_effort_defaults_to_low() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert_eq!(cfg.current_effort(), ReasoningEffortMirror::Low);
    }

    /// `set_effort` 写入新值,`current_effort()` 立即读到。
    #[test]
    fn agent_config_set_effort_updates_current() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        cfg.set_effort(ReasoningEffortMirror::High);
        assert_eq!(cfg.current_effort(), ReasoningEffortMirror::High);
        cfg.set_effort(ReasoningEffortMirror::Medium);
        assert_eq!(cfg.current_effort(), ReasoningEffortMirror::Medium);
    }

    /// `Clone` 后两副本共享同一把 RwLock —— 镜像 model / permission_mode
    /// 的 pattern,让 `submission_loop` 多处持有 `cfg.clone()` 后,
    /// `/effort` 切换对所有副本都可见。
    #[test]
    fn agent_config_clone_shares_effort_rwlock() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_effort(ReasoningEffortMirror::High);
        assert_eq!(
            cfg2.current_effort(),
            ReasoningEffortMirror::High,
            "clone 必须能观察到 effort 写入"
        );
    }

    /// `Clone` 后两副本共享 workspace RwLock —— worktree 热切换对所有
    /// `cfg.clone()` 持有者可见。
    #[test]
    fn agent_config_clone_shares_workspace_rwlock() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp/a");
        let cfg2 = cfg.clone();
        cfg.set_workspace("/tmp/b");
        assert_eq!(cfg2.current_workspace(), PathBuf::from("/tmp/b"));
    }

    /// worktree 状态默认可为空。
    #[test]
    fn agent_config_worktree_defaults_to_none() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert!(cfg.worktree_state().is_none());
    }

    // ── v1.2 P1-12: token budget ───────────────────────────────────────

    /// `AgentConfig::with_token_budget` 设置后 `current_token_budget` 读到。
    #[test]
    fn agent_config_with_token_budget_roundtrips() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp").with_token_budget(Some(42));
        assert_eq!(cfg.current_token_budget(), Some(42));
        let cfg2 = AgentConfig::new("anthropic/x", "/tmp").with_token_budget(None);
        assert_eq!(cfg2.current_token_budget(), None);
    }

    /// `set_token_budget` 热重载路径,Clone 后副本共享同一把 RwLock。
    #[test]
    fn agent_config_set_token_budget_shared_via_clone() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_token_budget(Some(100));
        assert_eq!(
            cfg2.current_token_budget(),
            Some(100),
            "clone 必须能观察到 token_budget 写入"
        );
    }

    /// `session_budget_exceeded`:无预算永不超过;有预算按总量判定。
    #[test]
    fn agent_config_session_budget_exceeded_logic() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        // 无预算:即使累计很大也不过。
        cfg.add_session_usage(&reflect_protocol::TokenUsage {
            total_tokens: 1_000_000,
            ..Default::default()
        });
        assert!(!cfg.session_budget_exceeded());
        // 设预算 500,累计 1_000_000 → 超过。
        cfg.set_token_budget(Some(500));
        assert!(cfg.session_budget_exceeded());
    }

    /// `add_session_usage`:累加 input/output/total;saturating 防溢出。
    #[test]
    fn agent_config_add_session_usage_accumulates() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        cfg.add_session_usage(&reflect_protocol::TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..Default::default()
        });
        cfg.add_session_usage(&reflect_protocol::TokenUsage {
            input_tokens: 50,
            output_tokens: 10,
            total_tokens: 60,
            ..Default::default()
        });
        let u = cfg.current_session_usage();
        assert_eq!(u.input_tokens, 150);
        assert_eq!(u.output_tokens, 30);
        assert_eq!(u.total_tokens, 180);
    }

    /// `shared_tool_context` 把 session_usage / token_budget /
    /// context_window_size 句柄共享给 `ToolContext`(get_context_remaining 工具读)。
    #[test]
    fn agent_config_shared_tool_context_shares_usage_handles() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp").with_token_budget(Some(1_000));
        *cfg.context_window_size.write() = Some(200_000);
        let ctx = cfg.shared_tool_context();
        // 写 cfg 的 session_usage,ctx 的句柄能读到(同一把锁)。
        cfg.add_session_usage(&reflect_protocol::TokenUsage {
            total_tokens: 77,
            ..Default::default()
        });
        assert_eq!(ctx.session_usage.read().total_tokens, 77);
        assert_eq!(*ctx.token_budget.read(), Some(1_000));
        assert_eq!(*ctx.context_window_size.read(), Some(200_000));
    }

    // ── v1.2 P1-12(已有-B): force_compact 标志 ───────────────────────

    /// 默认不强制压缩。
    #[test]
    fn force_compact_defaults_false() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert!(!cfg.take_force_compact());
    }

    /// `request_force_compact` 置 true,`take_force_compact` 读到并清零。
    #[test]
    fn force_compact_request_then_take_clears() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        cfg.request_force_compact();
        assert!(cfg.take_force_compact(), "should read true after request");
        assert!(
            !cfg.take_force_compact(),
            "take should clear the flag (second read is false)"
        );
    }

    /// Clone 后两副本共享同一把 RwLock(`/compact` 在 submission_loop 写,
    /// pre_loop 在 NodeContext 读)。
    #[test]
    fn force_compact_shared_via_clone() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.request_force_compact();
        assert!(
            cfg2.take_force_compact(),
            "clone must observe the force_compact write"
        );
    }

    // ── v1.x: max_iterations ─────────────────────────────────────────

    /// `AgentConfig::current_max_iterations` 默认 32。
    #[test]
    fn agent_config_max_iterations_defaults_to_32() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert_eq!(cfg.current_max_iterations(), 32);
    }

    /// `with_max_iterations` 构造器 + `current_max_iterations` 读。
    #[test]
    fn agent_config_with_max_iterations_roundtrips() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp").with_max_iterations(16);
        assert_eq!(cfg.current_max_iterations(), 16);
    }

    /// `set_max_iterations` 热重载路径,Clone 后副本共享同一把 RwLock。
    #[test]
    fn agent_config_set_max_iterations_shared_via_clone() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        let cfg2 = cfg.clone();
        cfg.set_max_iterations(5);
        assert_eq!(
            cfg2.current_max_iterations(),
            5,
            "clone 必须能观察到 max_iterations 写入"
        );
    }

    // ── v1.x:session_id 注入(让 TUI / 持久化 caller 预分配 id) ──────

    #[test]
    fn agent_config_session_id_defaults_to_none() {
        let cfg = AgentConfig::new("anthropic/x", "/tmp");
        assert!(cfg.session_id.is_none(), "默认不注入 session_id");
    }

    #[test]
    fn agent_config_with_session_id_roundtrips() {
        let id = ThreadId::new();
        let cfg = AgentConfig::new("anthropic/x", "/tmp").with_session_id(id);
        assert_eq!(cfg.session_id, Some(id), "with_session_id 应注入 id");
    }
}
