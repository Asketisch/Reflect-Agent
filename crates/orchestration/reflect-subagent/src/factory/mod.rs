//! `SubAgentFactory` —— 在父上下文中 spawn 子 `AgentThread`。
//!
//! 以 `Arc<SubAgentFactory>` 形式挂在 `NodeContext` 上,这样任意工具都能
//! 提交子 `Submission`,无需直接访问父级的 submission channel。
//!
//! 并发上限在此处执行:factory 持有一个 `AtomicU8` 计数器表示当前
//! in-flight(同时存活)的 spawn 数,每次 `spawn()` 递增,达到
//! [`crate::MAX_DEPTH`] 时拒绝 spawn。`SpawnedChild` 析构或
//! `collect_result` 终态时递减,保证长 session 不会累积到上限。
//! 计数器与子 factory 共享,这样嵌套的孙级 subagent 也共用同一深度预算。
//!
//! v0.2.2 起 `default_model` 用 `parking_lot::Mutex<String>` 包裹,允许
//! `reflect-exec::handle_reload` 在用户编辑 `~/.reflect/config.toml` 时
//! 通过 `set_default_model` 实时更新 —— 已 spawn 的子 agent 通过共享
//! `Arc<ModelRegistry>` 自动跟随 client 切换;**新 spawn 的子 agent**
//! 拿新 default。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use reflect_core::AgentThread;
use reflect_core::config::AgentConfig;
use reflect_llm::{ChatMessage, SharedModelRegistry};
use reflect_protocol::{RolloutRecord, Submission, ThreadId, TokenUsage};
use reflect_recovery::SubagentRegistry;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::data_transfer::DataTransferConfig;
use crate::error::SubAgentError;
use crate::spec::SubAgentSpec;
use crate::worker_registry::build_worker_tool_registry;

/// 共享 factory:克隆成本低,所有克隆体共享同一并发计数器。
pub struct SubAgentFactory {
    /// 共享的并发 in-flight 计数器;每次 `spawn()` 递增,`SpawnedChild`
    /// 析构或 `collect_result` 完成时递减。达到 [`crate::MAX_DEPTH`]
    /// 时 `spawn` 拒绝。
    in_flight: Arc<AtomicU8>,
    /// 父 thread id(用于 `RolloutRecord::Fork` 的 `parent_session_id`)。
    parent_session_id: ThreadId,
    /// 父级模型规格 —— `spec.model` 为 `None` 时用作默认值。
    /// v0.2.2: `Mutex<String>` 让 `handle_reload` 可写;`spawn` 时克隆
    /// 当前值快照进子 `AgentConfig.model`。
    default_model: Mutex<String>,
    /// 父级 registry(与子 agent 共享,使之能复用 API client)。
    registry: SharedModelRegistry,
    /// 子 agent 专用 registry(可选)。
    /// `Some` 时,子 agent 用此 registry 而非父级 registry,
    /// 允许 subagent 拥有独立的 base_url + api_key + model。
    /// `Mutex` 包裹让 `set_child_registry` 走 `&self`(对齐 `set_default_model`
    /// 模式),`Arc<Factory>` clone 后所有副本都看到同一份 registry;
    /// `reflect-exec::handle_reload` 热重载 `[subagent_providers]` 时替换。
    child_registry: Mutex<Option<SharedModelRegistry>>,
    /// v1.x 功能 6:父 agent 的 skills catalog(可选)。`None` 时 child 用
    /// 空 catalog(向后兼容,M5 v0 行为);`Some` 时注入到 child 的 M4,
    /// 让 subagent 也能 LoadSkill / 使用 skill 工具。`spawn` 时若
    /// `spec.allowed_skills` 非空,会克隆一份独立 catalog 并设置白名单
    /// (避免共享 catalog 的白名单在并发 spawn 间互相污染)。
    parent_skills: Mutex<Option<Arc<reflect_skills::SkillsCatalog>>>,
    /// 父级 tool registry —— 子 agent 接收其过滤后的视图。
    parent_tools: Arc<ToolRegistry>,
    /// 父级 cancel token —— 子 agent 继承它。
    cancel: CancellationToken,
    /// 父级 recorder(可选),用于写入 `Fork` 记录 + 创建子 agent JSONL 文件。
    recorder: Option<Arc<dyn reflect_protocol::RolloutRecorder>>,
    /// M5 v0:子 agent 仅获得一份最小 `AgentConfig`(无 M4 依赖)。若日后
    /// 需要子 agent 共享父级的 compactor / memory / skills,
    /// 在此处添加 `parent_m4: Option<M4Deps>` 并向下传递到子 agent。
    _no_parent_m4: (),
    /// v1.0.0-rc2: plugin 提供的 sub-agent spec —— key 是 plugin id,
    /// value 是该 plugin 注册的所有 spec。`PluginManager::load` 调
    /// `register_plugin_spec`;`unload` 调 `take_plugin_specs` 反注册。
    plugin_specs: Mutex<HashMap<String, Vec<SubAgentSpec>>>,
    /// v1.1.0: 运行时由 `TaskManager::sync_team_specs` 注入的动态 spec —— key 是
    /// `role`,value 是 spec。**不**被 `child_factory` 继承,与 `plugin_specs`
    /// 同语义(子 factory 仅继承 spawn 自身需要的 spec,不在自己身上管理)。
    ///
    /// 用途:Phase 3 起 `TeamCreate` 把团队成员的 `TeamMemberSpec` 转
    /// `SubAgentSpec` 注入到本字段,后续 `call_<role>` 即可 spawn。Phase 6
    /// TUI 任务面板直接消费 `list_specs` 渲染团队成员列表。
    dynamic_specs: Mutex<HashMap<String, SubAgentSpec>>,
    /// v1.1.0 Phase 6 P0:跨 turn 共享的子代理调用注册表。
    /// `None` 表示未接入(默认 / 单测);`reflect-exec::bootstrap_m5`
    /// 通过 `set_subagent_registry` 注入 `M4Deps` 的同一 Arc,
    /// `CallSubAgentTool::execute` 写,`pre_loop` 读 + 渲染成
    /// `<system-reminder>`。`Mutex` 包裹让 `set_subagent_registry`
    /// 走 `&self`(对齐 `set_default_model` 模式),`Arc<Factory>` clone
    /// 后所有副本都看到同一份 registry。
    subagent_registry: Mutex<Option<Arc<SubagentRegistry>>>,
    /// v1.1.0 Phase 4:coordinator 模式开关。`true` 时 `spawn` 走
    /// `build_worker_tool_registry`(从 `parent_tools` 排除 `INTERNAL_WORKER_TOOLS`),
    /// 替代默认的 `spec.allowed_tools` 过滤。`AtomicBool` 而非 `Mutex<bool>`:
    /// spawn 热路径只读,reload 偶写,无锁竞争。
    coordinator_mode: Arc<AtomicBool>,
    /// v1.1.0 Phase 4:coordinator 模式启用时,spawn 时附加到子 agent
    /// user input 末尾的「Coordinator Principle」reminder 文本。`None`
    /// 时不附加。`Mutex<Option<String>>` 让 `set_coordinator_mode` 走 `&self`
    /// (对齐 `set_default_model` 模式),子 factory 通过 clone 共享。
    coordinator_footer: Mutex<Option<String>>,
    /// P2 `git-worktree-auto`:coordinator 模式下 per-worker worktree 规划器。
    /// `None`(默认)= worker 不隔离,沿用 `.` 工作区(向后兼容);`Some` 时
    /// spawn 走 `ensure_for_task(child_session_id)` 给每个 worker 建独立 worktree,
    /// 子 `AgentConfig.workspace` 指向该 worktree 路径。
    worktree_coordinator: Mutex<Option<Arc<reflect_tools::WorktreeCoordinator>>>,
    /// v1.2 P1:父 agent 的 telemetry sink(可选)。`None`(默认)= 子 agent
    /// 不落库(向后兼容);`Some` 时 `spawn` 把它 clone 进子 `AgentConfig.telemetry`,
    /// 让子 agent 内的 `model_call` 自动复用 reflect-core 既有落库逻辑
    /// (`query_source = "main_turn"`)。对齐 `parent_skills` / `subagent_registry`
    /// 的 `Mutex<Option<Arc<_>>>` + setter 模式。
    telemetry: Mutex<Option<Arc<reflect_telemetry::TelemetrySink>>>,
}

impl std::fmt::Debug for SubAgentFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentFactory")
            .field("parent_session_id", &self.parent_session_id)
            .field("default_model", &self.default_model.lock().clone())
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .finish()
    }
}

impl SubAgentFactory {
    /// 在给定父 thread 下构造一个 factory。
    pub fn new(
        parent_session_id: ThreadId,
        default_model: impl Into<String>,
        registry: SharedModelRegistry,
        child_registry: Option<SharedModelRegistry>,
        parent_tools: Arc<ToolRegistry>,
        cancel: CancellationToken,
        recorder: Option<Arc<dyn reflect_protocol::RolloutRecorder>>,
    ) -> Self {
        Self {
            in_flight: Arc::new(AtomicU8::new(0)),
            parent_session_id,
            default_model: Mutex::new(default_model.into()),
            registry,
            child_registry: Mutex::new(child_registry),
            parent_skills: Mutex::new(None),
            parent_tools,
            cancel,
            recorder,
            _no_parent_m4: (),
            plugin_specs: Mutex::new(HashMap::new()),
            dynamic_specs: Mutex::new(HashMap::new()),
            subagent_registry: Mutex::new(None),
            coordinator_mode: Arc::new(AtomicBool::new(false)),
            coordinator_footer: Mutex::new(None),
            worktree_coordinator: Mutex::new(None),
            telemetry: Mutex::new(None),
        }
    }

    /// 当前并发 in-flight 数(父级为 0,每次 spawn +1,
    /// `SpawnedChild` 析构 / `collect_result` 完成时 -1)。
    pub fn depth(&self) -> u8 {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// 父 session 的 thread id —— coordinator scratchpad 路径与 reload 复用。
    pub fn parent_session_id(&self) -> ThreadId {
        self.parent_session_id
    }

    /// 读当前 default model spec。`spawn` 时和测试窥探都用。
    pub fn default_model(&self) -> String {
        self.default_model.lock().clone()
    }

    /// 写入新 default model spec。仅供 `reflect-exec::handle_reload` 在
    /// 检测到 `~/.reflect/config.toml` 变更后调用 —— 已 spawn 子 agent
    /// 不受影响(它们已经快照);后续 `spawn()` 拿新值。
    pub fn set_default_model(&self, new_spec: impl Into<String>) {
        *self.default_model.lock() = new_spec.into();
    }

    // ── v1.x: child registry 热重载(独立 base_url + api_key) ──────────

    /// 写入/替换 child 专用 registry。仅供 `reflect-exec::handle_reload`
    /// 在检测到 `[subagent_providers]` 段变更后调用 —— 重建独立 registry
    /// 并替换;后续 `spawn()` 拿新 registry。`None` 清除 → 回退父共享
    /// registry(向后兼容)。对齐 `set_default_model` 的 `&self` 模式。
    pub fn set_child_registry(&self, registry: Option<SharedModelRegistry>) {
        *self.child_registry.lock() = registry;
    }

    /// 读当前 child registry 快照(若有)。`spawn` 与测试窥探用。
    pub fn current_child_registry(&self) -> Option<SharedModelRegistry> {
        self.child_registry.lock().clone()
    }

    // ── v1.x 功能 6: parent skills 注入(subagent 支持 skill) ──────────

    /// 注入父 agent 的 skills catalog。仅供 `reflect-exec::bootstrap_m5`
    /// 在 M4Deps 构造后调一次 —— factory 与父 M4 持有同一 Arc,subagent
    /// spawn 时把它放进 child 的 M4,让子 agent 也能 LoadSkill / 使用 skill。
    pub fn set_parent_skills(&self, catalog: Arc<reflect_skills::SkillsCatalog>) {
        *self.parent_skills.lock() = Some(catalog);
    }

    // ── v1.2 P1: telemetry sink 注入(子 agent 落库) ─────────────────────

    /// 注入父 agent 的 telemetry sink。仅供 `reflect-exec::bootstrap_m5`
    /// 在 sink 构造后调一次 —— `spawn` 时把它 clone 进子 `AgentConfig.telemetry`,
    /// 让子 agent 内的 `model_call` 复用 reflect-core 既有落库逻辑。
    /// `None`(默认)= 子 agent 不落库(向后兼容)。对齐 `set_parent_skills`
    /// 的 `&self` 模式,`Arc<Factory>` clone 后所有副本共享同一 sink。
    pub fn set_telemetry(&self, sink: Option<Arc<reflect_telemetry::TelemetrySink>>) {
        *self.telemetry.lock() = sink;
    }

    /// 读当前 telemetry sink 快照(若有)。`spawn` 用。
    pub fn current_telemetry(&self) -> Option<Arc<reflect_telemetry::TelemetrySink>> {
        self.telemetry.lock().clone()
    }

    // ── v1.1.0 Phase 6 P0: subagent registry 共享 ───────────────────────

    /// 注入跨 turn 共享的子代理注册表。仅供 `reflect-exec::bootstrap_m5`
    /// 在 `M4Deps` 构造后调一次 —— factory 与 M4Deps 持有同一 Arc,
    /// `CallSubAgentTool::execute` 写,`pre_loop` 读。
    pub fn set_subagent_registry(&self, registry: Arc<SubagentRegistry>) {
        *self.subagent_registry.lock() = Some(registry);
    }

    /// 读当前注册表(若有)。给 `CallSubAgentTool::execute` 用。
    pub fn subagent_registry(&self) -> Option<Arc<SubagentRegistry>> {
        self.subagent_registry.lock().clone()
    }

    // ── v1.1.0 Phase 4: coordinator mode 注入 ─────────────────────────

    /// 设置 coordinator 模式开关与 footer 文本。仅供
    /// `reflect-exec::bootstrap_m4` 在 `coord_cfg.enabled` 时调一次。
    ///
    /// - `enabled = true` → `spawn` 走 `build_worker_tool_registry`,子 agent
    ///   拿不到 `INTERNAL_WORKER_TOOLS`(`TeamCreate` / `TeamDelete` /
    ///   `send_message`)。
    /// - `enabled = false` → 回退原 `spec.allowed_tools` 路径(回归保护)。
    /// - `footer = Some(text)` → spawn 时把 `[Coordinator Principle]` 段附加
    ///   到 `combined_user_input` 末尾,提醒 worker 独立综合结果。
    ///
    /// 子 factory 通过 `child_factory` 共享同一 `Arc<AtomicBool>` + clone
    /// 当前 footer,与 `subagent_registry` 模式一致。
    pub fn set_coordinator_mode(&self, enabled: bool, footer: Option<String>) {
        self.coordinator_mode.store(enabled, Ordering::SeqCst);
        *self.coordinator_footer.lock() = footer;
    }

    /// 当前 coordinator 模式开关(供测试与诊断)。
    pub fn is_coordinator_mode(&self) -> bool {
        self.coordinator_mode.load(Ordering::SeqCst)
    }

    /// 当前 coordinator footer 文本(供测试与诊断)。
    pub fn coordinator_footer(&self) -> Option<String> {
        self.coordinator_footer.lock().clone()
    }

    /// P2 `git-worktree-auto`:注入 per-worker worktree 规划器。
    /// 配合 `set_coordinator_mode(true, …)`,`spawn` 会给每个 worker 建独立
    /// worktree 并把子工作区指向它(真实 git 隔离)。`None` 关闭隔离。
    pub fn set_worktree_coordinator(&self, c: Option<Arc<reflect_tools::WorktreeCoordinator>>) {
        *self.worktree_coordinator.lock() = c;
    }

    /// 当前 worktree 规划器(供测试与诊断)。
    pub fn worktree_coordinator(&self) -> Option<Arc<reflect_tools::WorktreeCoordinator>> {
        self.worktree_coordinator.lock().clone()
    }

    // ── v1.0.0-rc2: plugin sub-agent 注册 ────────────────────────────

    /// 注册一个 plugin 提供的 sub-agent spec。
    ///
    /// `PluginManager::load` 在 plugin load 时调此方法,把 spec 存到
    /// factory 的 plugin 命名空间下;后续 `PluginManager` 还会构造对应的
    /// `CallSubAgentTool` 实例并 `tools.register_plugin_tool()` 挂到
    /// `ToolRegistry`。`spawn` 本身不查这个表 —— 它通过 `CallSubAgentTool`
    /// 持有 spec 的引用直接 spawn,避免一次额外查表。
    ///
    /// 反注册:`take_plugin_specs(plugin_id)` 在 `PluginManager::unload`
    /// 时拿回所有 spec,反注册对应的 `CallSubAgentTool`。
    pub fn register_plugin_spec(&self, plugin_id: &str, spec: SubAgentSpec) {
        if let Err(e) = spec.validate() {
            tracing::warn!(
                plugin = %plugin_id,
                role = %spec.role,
                error = %e,
                "plugin agent spec 校验失败,跳过"
            );
            return;
        }
        let mut map = self.plugin_specs.lock();
        map.entry(plugin_id.to_string()).or_default().push(spec);
    }

    /// 取出并移除某个 plugin 的所有 spec。返回被移除的 specs,给 caller
    /// 反注册对应的 `CallSubAgentTool`(`ToolRegistry::unregister(name)`)。
    pub fn take_plugin_specs(&self, plugin_id: &str) -> Vec<SubAgentSpec> {
        self.plugin_specs
            .lock()
            .remove(plugin_id)
            .unwrap_or_default()
    }

    /// 只读列出某个 plugin 注册的 specs(不消耗)—— 给 UI / 测试用。
    pub fn plugin_specs_for(&self, plugin_id: &str) -> Vec<SubAgentSpec> {
        self.plugin_specs
            .lock()
            .get(plugin_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 列出所有已注册 plugin id —— 字典序,保证测试与 UI 渲染稳定。
    pub fn registered_plugin_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugin_specs.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    // ── v1.1.0: dynamic specs (TeamCreate / CLI / 运行时注入) ────────

    /// 替换全部 dynamic specs —— 由 `TaskManager::sync_team_specs` 在每次
    /// 团队成员变更后调用。`specs` 中 `validate` 失败的项跳过 + `warn`,
    /// 与 `register_plugin_spec` 风格一致,保证失败的 spec 不污染 factory。
    pub fn set_specs(&self, specs: Vec<SubAgentSpec>) {
        let mut map = self.dynamic_specs.lock();
        map.clear();
        for spec in specs {
            if let Err(e) = spec.validate() {
                warn!(
                    role = %spec.role,
                    error = %e,
                    "sync_team_specs: spec validate 失败,跳过"
                );
                continue;
            }
            map.insert(spec.role.clone(), spec);
        }
    }

    /// 追加单个 spec。返回 `true` = 成功;`false` = `validate` 失败(spec
    /// 未被注入)。给 CLI `reflect task team-sync` 子命令用。
    pub fn add_spec(&self, spec: SubAgentSpec) -> bool {
        if let Err(e) = spec.validate() {
            warn!(role = %spec.role, error = %e, "add_spec: spec validate 失败");
            return false;
        }
        self.dynamic_specs.lock().insert(spec.role.clone(), spec);
        true
    }

    /// 按 role 移除 spec。返回 `true` = 有移除,`false` = 不存在。
    pub fn remove_spec(&self, role: &str) -> bool {
        self.dynamic_specs.lock().remove(role).is_some()
    }

    /// 列出全部 dynamic specs —— 按 `(role, name)` 字典序对,供 Phase 6
    /// TUI 任务面板渲染团队成员列表,以及 CLI `reflect task team-sync --list`。
    pub fn list_specs(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .dynamic_specs
            .lock()
            .values()
            .map(|s| (s.role.clone(), s.name.clone()))
            .collect();
        pairs.sort();
        pairs
    }

    /// 按 role 取单个 spec 副本。
    pub fn get_spec(&self, role: &str) -> Option<SubAgentSpec> {
        self.dynamic_specs.lock().get(role).cloned()
    }

    /// 为子 thread 构造一个共享本 factory 深度计数器的 factory。
    /// 由 [`spawn`] 使用 —— 让子 agent 拥有相同深度预算,从而其 spawn 也被计入。
    pub fn child_factory(&self) -> Self {
        Self {
            in_flight: Arc::clone(&self.in_flight),
            parent_session_id: self.parent_session_id,
            default_model: Mutex::new(self.default_model.lock().clone()),
            registry: self.registry.clone(),
            // 子 factory 继承父级的 child_registry —— 孙级 subagent 也走独立凭证。
            // 取父级当前快照(独立 Mutex,后续父子互不影响热重载)。
            child_registry: Mutex::new(self.child_registry.lock().clone()),
            // 子 factory 继承父级 parent_skills —— 孙级也能用 skill。
            parent_skills: Mutex::new(self.parent_skills.lock().clone()),
            parent_tools: self.parent_tools.clone(),
            cancel: self.cancel.clone(),
            recorder: self.recorder.clone(),
            _no_parent_m4: (),
            // 子 factory 不继承 plugin_specs —— 子 agent 不需要管理
            // 父级 plugin;`PluginManager` 直接在父 factory 上操作。
            plugin_specs: Mutex::new(HashMap::new()),
            // dynamic_specs 同样不继承 —— spawn 时由调用方把需要的 spec
            // 显式 `add_spec` 到子 factory,避免子 agent 误用父级团队成员。
            dynamic_specs: Mutex::new(HashMap::new()),
            // registry 在父子间共享同一 Arc(若父注入了),保证孙级
            // 子代理也能被父级 pre_loop 看到。
            subagent_registry: Mutex::new(self.subagent_registry.lock().clone()),
            // coordinator 模式在父子间共享 —— 父启用时,子 spawn 出的孙级
            // 也走 worker tool 白名单;`Arc<AtomicBool>` 多读单写无锁竞争。
            coordinator_mode: self.coordinator_mode.clone(),
            // footer 走 clone 当前值快照(子 factory 后可独立更新)。
            coordinator_footer: Mutex::new(self.coordinator_footer.lock().clone()),
            // worktree 规划器在父子间共享同一 Arc(若父注入了),让孙级
            // spawn 也走 worktree 隔离。
            worktree_coordinator: Mutex::new(self.worktree_coordinator.lock().clone()),
            // v1.2 P1:telemetry sink 在父子间共享 —— 孙级 subagent 也复用
            // 父级同一 sink(同一 session_id,落库记录连续)。
            telemetry: Mutex::new(self.telemetry.lock().clone()),
        }
    }

    /// 为 `spec` spawn 一个子 thread。返回子 agent 的 `TurnHandle`、
    /// 新的子 `ThreadId`,以及一个子 factory 用于进一步嵌套。
    /// 若达到深度上限,在任何 `Submission` 入队**之前**返回
    /// [`SubAgentError::MaxDepthExceeded`]。
    pub async fn spawn(
        &self,
        spec: SubAgentSpec,
        parent_tail: Vec<ChatMessage>,
        user_prompt: String,
    ) -> Result<SpawnedChild, SubAgentError> {
        spec.validate().map_err(SubAgentError::SpecInvalid)?;

        // 原子地预留一个并发槽位;若已到上限则拒绝。
        let prev = self.in_flight.fetch_add(1, Ordering::SeqCst);
        if prev >= crate::MAX_DEPTH {
            // 回滚计数,避免卡在上限处无法恢复。
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(SubAgentError::MaxDepthExceeded {
                max: crate::MAX_DEPTH,
            });
        }

        // 构造子 session —— 新的 `ThreadId` 用于血缘追踪。
        //
        // v1.x:子代理**不再**写独立 JSONL 文件(此前每次 `CallSubAgentTool`
        // 都新建一个子 session 文件,导致"一次对话产生多个 session",污染
        // `/session` 列表)。血缘关系已由下面写入**父** recorder 的 `Fork`
        // record 记录(branch_name / parent_session_id),无需子文件。子代理
        // 在内存里跑完即丢,符合"子代理是父会话的临时工具调用"语义。
        let child_thread_id = ThreadId::new();
        let child_recorder: Option<Arc<dyn reflect_protocol::RolloutRecorder>> = None;

        // 向父 recorder(若有)写入 `Fork` 记录,这样恢复时可查询父子关系。
        if let Some(rec) = self.recorder.as_ref() {
            let _ = rec
                .record(RolloutRecord::Fork {
                    parent_session_id: self.parent_session_id,
                    branch_name: spec.role.clone(),
                })
                .await;
        }

        // 构造仅包含 `allowed_tools` 的过滤后 tool registry。
        //
        // v1.1.0 Phase 4 改造:coordinator 模式启用时,不走 `spec.allowed_tools`,
        // 而是走 `reflect_task::coordinator::build_worker_tool_registry` 从父
        // registry 排除 `INTERNAL_WORKER_TOOLS`(`TeamCreate` / `TeamDelete` /
        // `send_message`)。worker 不能自行创建 / 删除团队,
        // 也不能反向发消息给 coordinator(留 v1.2)。
        //
        // 注:`Arc<ToolRegistry>` 共享父级 —— 即便 worker 之后做 reload 触发
        // 子 registry 重建,父级主 session 的 plugin 加载 / MCP server 启动
        // 等副作用仍由主 session 持有,worker 只读 filter 视图。
        let child_tools = Arc::new(ToolRegistry::default());
        if self.coordinator_mode.load(Ordering::SeqCst) {
            // coordinator 启用:从父 registry 排除 internal tools。
            let worker_registry = build_worker_tool_registry(&self.parent_tools);
            for name in worker_registry.list() {
                if let Some(t) = worker_registry.get(&name) {
                    child_tools.register(t);
                }
            }
        } else {
            // 默认路径:按 spec.allowed_tools 过滤。
            for name in &spec.allowed_tools {
                if let Some(t) = self.parent_tools.get(name) {
                    child_tools.register(t);
                }
            }
        }

        // 构造子 `AgentConfig` + thread。在 spawn 入口处快照 `default_model` —— reload 写锁不会和这里的读锁竞争(锁粒度 String 几纳秒)。
        // 在 spawn 入口取 —— reload 写锁不会和这里读锁竞争(锁粒度 String
        // 几纳秒)。
        let child_model = spec
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.lock().clone());
        // P2 `git-worktree-auto`:coordinator 模式 + worktree 规划器注入时,
        // 给当前 worker 建独立 worktree,子工作区指向它(真实 git 隔离)。
        // 失败仅 warn 并回退 `.`(不让 worktree 故障阻断 spawn)。
        let workspace = if self.coordinator_mode.load(Ordering::SeqCst) {
            if let Some(c) = self.worktree_coordinator.lock().clone() {
                // 用 child session id 作 task key(唯一),create_git=true 建真实 worktree。
                match c.ensure_for_task(&child_thread_id.to_string(), true) {
                    Ok(state) => state.worktree_path,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "worktree 隔离失败,worker 回退到当前工作区"
                        );
                        std::path::PathBuf::from(".")
                    }
                }
            } else {
                std::path::PathBuf::from(".")
            }
        } else {
            std::path::PathBuf::from(".")
        };
        let mut cfg = AgentConfig::new(child_model, workspace);

        // v1.x 功能 6:child M4 构造。即便没有 recorder,也构造一份 m4
        // (注入 skills catalog + subagent registry),让 subagent 拥有
        // skill 能力 + 完成记录 reminder。recorder 缺省时用 `default_m4_deps`
        // 的 None。
        let parent_skills = self.parent_skills.lock().clone();
        let mut m4 = reflect_core::config::default_m4_deps("subagent");
        // v1.1.0 review bug-1 (P0):注入父 factory 的同一 subagent registry Arc。
        if let Some(reg) = self.subagent_registry() {
            m4.subagent_registry = reg;
        }
        if let Some(rec) = child_recorder.clone() {
            m4.recorder = Some(rec);
        }
        // v1.x 功能 6:注入 skills。`spec.allowed_skills` 非空时,克隆一份
        // 独立 catalog 并设置白名单(避免共享 catalog 并发污染);
        // 空时直接共享父 catalog Arc(子 agent 继承全部 skill)。
        if let Some(parent_cat) = parent_skills {
            if spec.allowed_skills.is_empty() {
                m4.skills = parent_cat;
            } else {
                let allow: HashSet<String> = spec.allowed_skills.iter().cloned().collect();
                let cloned = clone_catalog_subset(&parent_cat, &allow);
                m4.skills = Arc::new(cloned);
            }
        }
        cfg.m4 = Some(m4);
        // v1.2 P1:注入父 telemetry sink,让子 agent 的 model_call 自动落库
        // (复用 reflect-core 既有逻辑,query_source 仍为 "main_turn")。
        if let Some(sink) = self.current_telemetry() {
            cfg.telemetry = Some(sink);
        }
        // 若配置了 child registry(独立的 subagent 凭证),则使用它;否则回退到父级共享 registry。
        let target_registry = self
            .child_registry
            .lock()
            .clone()
            .unwrap_or_else(|| self.registry.clone());
        // v1.x:per-subagent 迭代上限。`spec.max_turns` 存在时取与全局
        // 默认(32)的 `min`,更严格的优先;`None` 沿用全局默认,向后兼容。
        let global_max = cfg.current_max_iterations();
        let child_max = spec.max_turns.map_or(global_max, |n| n.min(global_max));
        cfg.set_max_iterations(child_max);
        let child_thread = AgentThread::new(cfg, target_registry, child_tools, None, None);

        // 子 agent 以单条 User 消息的形式接收 system prompt + role 标签 + user prompt。
        // 父级 context tail 当前未使用(M5 v0);它是预留接口,留给后续 M6 增强,
        // 让父级把筛选过的历史传给子 agent。
        let _ = parent_tail;
        let combined_user_input = build_spawn_user_input(
            &spec,
            &user_prompt,
            self.coordinator_mode.load(Ordering::SeqCst),
            self.coordinator_footer.lock().clone(),
        );
        let sub = Submission::user_input(combined_user_input);
        let handle = child_thread.submit(sub).await;
        Ok(SpawnedChild {
            session_id: child_thread_id,
            handle,
            data_transfer: spec.data_transfer,
            in_flight: Some(Arc::clone(&self.in_flight)),
        })
    }
}

/// 构造 spawn 时发给子 agent 的合并 user input。
/// coordinator 启用且 footer 非空时附加 `[Coordinator Principle]` 段。
pub(crate) fn build_spawn_user_input(
    spec: &SubAgentSpec,
    user_prompt: &str,
    coordinator_enabled: bool,
    footer: Option<String>,
) -> String {
    let mut combined = format!(
        "{}\n\n[Subagent role: {}]\n\n{}",
        spec.system_prompt, spec.name, user_prompt
    );
    if coordinator_enabled {
        if let Some(footer) = footer {
            if !footer.trim().is_empty() {
                combined.push_str("\n\n[Coordinator Principle]\n");
                combined.push_str(footer.trim());
                combined.push('\n');
            }
        }
    }
    combined
}

/// [`SubAgentFactory::spawn`] 返回的句柄 —— 调用方从 `handle` 排空事件,
/// 并将其传给 [`crate::data_transfer::extract_result`]。
///
/// 析构时自动递减 factory 的 in-flight 计数器(即便调用方忘记
/// `collect_result` 也保证释放槽位),同时 `collect_result_with_usage`
/// 在终态主动释放作为冗余保护。
pub struct SpawnedChild {
    pub session_id: ThreadId,
    pub handle: reflect_core::TurnHandle,
    pub data_transfer: DataTransferConfig,
    /// 父 factory 的并发计数器句柄。`Drop` 中释放,确保长 session
    /// 即便漏调 `collect_result` 也不会累积到 [`crate::MAX_DEPTH`]。
    ///
    /// 用 `Option<Arc<...>>` 而非裸 `Arc<...>`:让 `collect_result_with_usage`
    /// 可以通过 `Option::take` 安全地把所有权从 `self` 移走,避免
    /// `ManuallyDrop + ptr::read` 的内存泄漏风险;`Drop` 检查 `Some`
    /// 才 fetch_sub,已被 `take` 的句柄不会重复减。
    in_flight: Option<Arc<AtomicU8>>,
}

impl std::fmt::Debug for SpawnedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedChild")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if let Some(arc) = self.in_flight.take() {
            // `fetch_sub` 自然下溢到 0 是合法的 u8 —— 但若有代码 bug 让
            // 计数器减到 0 以下会静默 wrap,这里 saturating 防御。
            let prev = arc.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some(cur.saturating_sub(1))
            });
            if let Ok(prev) = prev {
                debug_assert!(prev > 0, "in_flight underflow: counter already 0");
            }
        }
    }
}

impl SpawnedChild {
    /// 排空事件直至 `TurnComplete`(或 channel 关闭),并用
    /// `data_transfer.result_extractor` 提取最终回答。
    /// 围绕 [`Self::collect_result_with_usage`] 的薄封装,保留 M5/M6 的签名。
    pub async fn collect_result(self) -> Result<String, SubAgentError> {
        self.collect_result_with_usage().await.map(|r| r.text)
    }

    /// v0.2.4: 同 [`Self::collect_result`] 的 drain 流程,额外捕获 LLM 上报的
    /// token usage 与 elapsed 毫秒数,供 `DiscussionOrchestrator` 把 usage
    /// 注入出站 `DiscussionMessage`(`M9 已知限制 #3`)。
    ///
    /// usage 捕获优先级:
    /// 1. `TurnComplete.usage` —— 终结事件携带的最权威 usage
    /// 2. 最后一个 `TokenCount` 事件 —— 当 `TurnComplete` 没 emit(早终止 /
    ///    channel 关闭)的兜底
    ///
    /// 都不存在时 `token_usage = None`,调用方应当 fallback 到 `Default`。
    pub async fn collect_result_with_usage(self) -> Result<SpawnedResult, SubAgentError> {
        // `SpawnedChild` 实现了 `Drop`,Rust 不允许部分 move。我们用
        // `Option::take` 安全地移走 `in_flight` Arc,然后用 `ManuallyDrop`
        // + `ptr::read` 拆出剩余字段(handle / data_transfer / session_id)。
        // 这样:
        // 1. in_flight Arc 的所有权被显式接管,函数末尾 drop(fetch_sub)。
        // 2. ManuallyDrop 包裹下,`SpawnedChild::drop` 不会被自动调,避免
        //    双重 fetch_sub —— 因为 in_flight 已被 Option::take 走了,
        //    即使 Drop 真跑了,`Some` 检查会跳过。
        use std::mem::ManuallyDrop;
        let mut me = ManuallyDrop::new(self);
        // 安全:Option::take 把 Some 替换为 None,后续字段单独 read。
        let in_flight_arc = me.in_flight.take(); // Option<Arc<AtomicU8>> → None
        let mut handle = unsafe { std::ptr::read(&me.handle) };
        let data_transfer = unsafe { std::ptr::read(&me.data_transfer) };
        let _session_id = unsafe { std::ptr::read(&me.session_id) };
        let started_at = Instant::now();
        let mut events = Vec::new();
        // 跟踪当前见过最权威的 usage 来源。
        let mut usage_from_turn_complete: Option<TokenUsage> = None;
        let mut usage_from_token_count: Option<TokenUsage> = None;
        while let Some(ev) = handle.next().await {
            match &ev.msg {
                reflect_protocol::EventMsg::TurnComplete(tc) => {
                    usage_from_turn_complete = Some(tc.usage.clone());
                }
                reflect_protocol::EventMsg::TokenCount(tc) => {
                    usage_from_token_count = Some(TokenUsage {
                        input_tokens: tc.input_tokens,
                        output_tokens: tc.output_tokens,
                        cached_tokens: tc.cached_tokens,
                        cache_write_tokens: tc.cache_write_tokens,
                        total_tokens: tc.total_tokens,
                    });
                }
                _ => {}
            }
            let is_terminal = matches!(
                ev.msg,
                reflect_protocol::EventMsg::TurnComplete(_)
                    | reflect_protocol::EventMsg::TurnAborted(_)
                    | reflect_protocol::EventMsg::ShutdownComplete
            );
            events.push(ev);
            if is_terminal {
                break;
            }
        }
        let text = crate::data_transfer::extract_result(&events, &data_transfer.result_extractor)
            .ok_or_else(|| SubAgentError::SpawnFailed("no result extractable".into()))?;
        let token_usage = usage_from_turn_complete.or(usage_from_token_count);
        // 释放 in_flight Arc(fetch_sub 一次),让 in-flight 槽位归位。
        if let Some(arc) = in_flight_arc {
            arc.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(SpawnedResult {
            text,
            token_usage,
            elapsed_ms: started_at.elapsed().as_millis() as u64,
        })
    }
}

/// v0.2.4: 一次 subagent spawn 的完整结果。
///
/// `text` 是按 `data_transfer.result_extractor` 抽出的最终回答文本;
/// `token_usage` 是 LLM 上报的 token 用量(优先取 `TurnComplete.usage`,
/// 兜底取最后一个 `TokenCount` 事件);`elapsed_ms` 是 drain 流的总耗时。
#[derive(Debug, Clone)]
pub struct SpawnedResult {
    pub text: String,
    pub token_usage: Option<TokenUsage>,
    pub elapsed_ms: u64,
}

// 内部 helper 已移除 —— 子 agent 的 AgentConfig 在上面直接填充。

/// v1.x 功能 6:从父 `SkillsCatalog` 克隆一份独立 catalog,只包含 `allow`
/// 白名单内的 skill。新 catalog 的 `activated` / `always_on` 复用父级语义
/// (always_on 基础工具集不变;activated 留空,让子 agent 自行 LoadSkill)。
/// 避免共享 catalog 的白名单在并发 spawn 间互相污染。
fn clone_catalog_subset(
    parent: &reflect_skills::SkillsCatalog,
    allow: &HashSet<String>,
) -> reflect_skills::SkillsCatalog {
    use reflect_skills::SkillsCatalog;
    let cloned = SkillsCatalog::new();
    for name in parent.names() {
        if allow.contains(&name) {
            if let Some(meta) = parent.get(&name) {
                cloned.insert(meta);
            }
        }
    }
    cloned
}

#[cfg(test)]
mod tests;
