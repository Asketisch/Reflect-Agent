//! Tool trait 及其伴随类型。
//!
//! `ToolError` 与 `ToolOutput` 定义在 `reflect-protocol`,目的是打破
//! `reflect-tools` ↔ `reflect-hooks` 之间的循环依赖。`Tool` trait
//! 与内建工具的实现保留在本 crate。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use reflect_protocol::{PermissionMode, ThreadId, TurnId};

use crate::approval::ApprovalGate;
use crate::worktree::SessionWorktreeState;

pub use reflect_protocol::ToolError;
pub use reflect_protocol::ToolOutput;

/// 模型可调用的一项能力。无状态(状态在 `ToolRegistry` 里)。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 稳定的工具名(如 `"bash"`)。
    fn name(&self) -> &str;

    /// 人 / 模型可读的描述,会发给模型。
    fn description(&self) -> &str;

    /// 描述入参的 JSON Schema。
    fn parameters_schema(&self) -> serde_json::Value;

    /// 若该工具的多次调用(以及与其他 concurrency-safe 工具)能安全并行
    /// 执行则返回 `true`。只读工具返回 `true`;有副作用的工具(文件变更、
    /// 网络、执行命令)返回 `false`。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// 调用该工具所需的权限。默认 `Auto` —— 不弹审批直接执行。有副作用
    /// 的工具(`bash`、`write`、`edit`)应当重写为 `Prompt`,使
    /// `ToolExecutionQueue` 走 M6 的 `ApprovalGate` 审批流程。
    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    /// Per-action 权限路由(v1.0.0-rc1+):多 action 单 tool(如 `ast`
    /// / `lsp`)用这个方法按具体 action 决定 `Auto` vs `Prompt`,避免
    /// 整体 tool 走 Prompt 阻塞 LLM 试探(`lsp` 的 read action 应该 Auto,
    /// `ast` 的 `search` 应该 Auto,但 `ast` 的 `replace` 应该 Prompt)。
    ///
    /// 默认 fallback 到 [`required_permission`](Self::required_permission)
    /// —— 单 action 工具不需要覆盖这个方法。
    fn action_permission(&self, _args: &serde_json::Value) -> PermissionMode {
        self.required_permission()
    }

    /// 执行该工具。`elapsed_ms` 由 queue 在返回的 `ToolOutput` 上填入
    /// (M2+);M1 工具返回 0。
    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;
}

/// 每次工具调用都会收到的上下文。
///
/// `ToolContext` **不**携带 `HookEngine` —— 引擎由 `ToolExecutionQueue`
/// 自己持有。需要感知 hook 状态的工具请读 `ToolContext::metadata`
/// (queue 在 `execute` 之前会把 `InjectMessage` 形式的提醒塞到该字段)。
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// 工作区根路径 —— 与 `AgentConfig.workspace` 共享同一把 `RwLock`,
    /// `EnterWorktreeTool` 可在会话内热切换。
    pub workspace: Arc<RwLock<PathBuf>>,
    /// 当前 worktree 会话状态;`None` 表示在主 workspace。
    pub worktree: Arc<RwLock<Option<SessionWorktreeState>>>,
    pub cancel: CancellationToken,
    pub timeout: Duration,
    pub call_id: String,
    /// 稳定的 session id(M3+)。
    pub session_id: ThreadId,
    /// 单 turn id(M3+)。
    pub turn_id: TurnId,
    /// 注入的环境变量(M3+;供 subagent 使用)。
    pub env: HashMap<String, String>,
    /// 本次调用生效的权限模式(M3+;可被 `HookDecision::PermissionOverride` 改写)。
    pub permission_mode: PermissionMode,
    /// 单次调用的临时元数据(M3+;hook 注入的 reminder 会塞到 `system_reminder` 键下)。
    pub metadata: serde_json::Value,
    /// v1.0.0-rc1+:可选的 approval gate 句柄,由 `ToolExecutionQueue`
    /// 在 `execute_single` 入口注入 `Some(Arc<ApprovalGate>)`,工具内
    /// 可以走 `gate.ask_tool(name, args, ...)` 主动触发 prompt-permission
    /// modal(典型场景:per-action 权限决策 —— 同 tool 不同 action 走不同
    /// approval 路径)。`None` 意味着 headless 模式(无 TUI),不主动 prompt。
    pub approval: Option<Arc<ApprovalGate>>,
    /// v1.2 P1-12:会话级累计 token 用量(跨 turn 累加)。与 `AgentConfig`
    /// / `NodeContext` 共享同一把 `Arc<RwLock<TokenUsage>>`。`model_call`
    /// 每次调用后累加;`get_context_remaining` 工具读它算剩余预算。
    /// `Arc::new(RwLock::new(TokenUsage::default()))` 默认值让无引擎上下文
    /// 的测试 / 单次调用也能工作(读到全零)。
    pub session_usage: Arc<RwLock<reflect_protocol::TokenUsage>>,
    /// v1.2 P1-12:会话 token 预算硬上限(共享句柄)。`get_context_remaining`
    /// 用它报告预算剩余。`None` = 无预算上限。
    pub token_budget: Arc<RwLock<Option<u64>>>,
    /// v1.2 P1-12:当前 model 的上下文窗口大小(token),引擎在
    /// `SessionConfigured` 时用 `reflect_llm::context_window_for` 算出后
    /// 写入(共享句柄,热重载切 model 后刷新)。`get_context_remaining`
    /// 用它做「已用 / 总量」的分母。`None` = 未知 model。
    pub context_window_size: Arc<RwLock<Option<u32>>>,
    /// v1.4 A3:工具输出流式增量回调。`None`(默认)= 工具不上报增量
    /// (零开销,所有既有工具行为不变);`Some` 时长时间运行的工具
    /// (bash 等)在执行期间逐段上报输出,由 `ToolExecutionQueue` 转发为
    /// `EventMsg::ToolCallOutputDelta`。回调参数:`(is_stderr, delta)`。
    pub progress: Option<ProgressSink>,
    /// v1.4 C1:工具事件转发器(subagent 编排用)。`None`(默认)= 无
    /// 转发(零开销);`Some` 时由 `tool_exec` 注入,`CallSubAgentTool`
    /// 把子代理中间事件包装为 `SubagentProgress` 事件经它发出。
    pub event_forwarder: Option<Arc<ToolEventForwarder>>,
    /// v1.4 C1:父会话历史尾部快照(最近若干条,由 `tool_exec` 注入)。
    /// 仅子代理编排工具(`CallSubAgentTool`)读取:按
    /// `DataTransferConfig.pass_context_messages` / 调用参数截取后传给
    /// `factory.spawn` 的 `parent_tail`。其余工具忽略。元素是协议层
    /// `ChatMessage` 的 JSON 形态,避免 reflect-tools 反向依赖 llm 层。
    pub parent_tail_json: Arc<RwLock<Vec<serde_json::Value>>>,
}

/// v1.4 C1:工具 → 引擎事件通道的转发器。`tool_exec` 在每批执行前构造
/// (持有 sub_id、event_tx 与父历史尾部快照),`execute_single` 注入每个
/// 调用的 `ToolContext`;工具用它发出自定义协议事件(子代理进度等)。
/// `try_send` 满则丢帧 —— 事件是观测性增益,不阻塞工具执行。
#[derive(Clone)]
pub struct ToolEventForwarder {
    /// 事件归属的 submission id(`Event.id`)。
    pub sub_id: String,
    tx: tokio::sync::mpsc::Sender<reflect_protocol::Event>,
    /// 父会话历史尾部快照(`state.messages` 最近若干条的 JSON 形态)。
    /// `CallSubAgentTool` 经 `ctx.parent_tail_json` 读取。
    pub parent_tail_json: Arc<RwLock<Vec<serde_json::Value>>>,
}

impl std::fmt::Debug for ToolEventForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEventForwarder")
            .field("sub_id", &self.sub_id)
            .finish_non_exhaustive()
    }
}

impl ToolEventForwarder {
    pub fn new(
        sub_id: impl Into<String>,
        tx: tokio::sync::mpsc::Sender<reflect_protocol::Event>,
        parent_tail_json: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            sub_id: sub_id.into(),
            tx,
            parent_tail_json: Arc::new(RwLock::new(parent_tail_json)),
        }
    }

    /// 发出一条协议事件(非阻塞;通道满则丢弃)。
    pub fn forward(&self, msg: reflect_protocol::EventMsg) {
        let _ = self
            .tx
            .try_send(reflect_protocol::Event::new(self.sub_id.clone(), msg));
    }

    /// 底层通道句柄(输出增量等 `EVENT_ID_NONE` 事件直发用)。
    pub fn raw_sender(&self) -> tokio::sync::mpsc::Sender<reflect_protocol::Event> {
        self.tx.clone()
    }
}

/// v1.4 A3:进度回调句柄。newtype 包裹 `Arc<dyn Fn>` —— 让
/// `ToolContext` 的 `Debug` / `Clone` derive 保持可用(裸 trait 对象
/// 两者都不可派生)。
#[derive(Clone)]
pub struct ProgressSink(pub Arc<dyn ToolProgressFn>);

/// 进度回调的函数对象形态:`(is_stderr, delta) -> ()`。
/// 单独定义 trait alias 形态,避免 clippy type_complexity。
pub trait ToolProgressFn: Fn(bool, &str) + Send + Sync {}
impl<T: Fn(bool, &str) + Send + Sync> ToolProgressFn for T {}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink(..)")
    }
}

impl ProgressSink {
    /// 构造一个进度回调句柄(便捷:`Arc<dyn Fn>` 自动满足 `ToolProgressFn`)。
    pub fn new(f: impl Fn(bool, &str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// 上报一段增量。`is_stderr` 区分标准错误流;`delta` 为原始分片。
    pub fn emit(&self, is_stderr: bool, delta: &str) {
        (self.0)(is_stderr, delta);
    }
}

impl ToolContext {
    /// 从路径构造 `ToolContext`(测试 / 单次调用用)。
    pub fn for_workspace(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: Arc::new(RwLock::new(workspace.into())),
            ..Default::default()
        }
    }

    /// 与 `AgentConfig` 共享 workspace / worktree 句柄(bootstrap 用)。
    pub fn with_shared(
        workspace: Arc<RwLock<PathBuf>>,
        worktree: Arc<RwLock<Option<SessionWorktreeState>>>,
    ) -> Self {
        Self {
            workspace,
            worktree,
            ..Default::default()
        }
    }

    /// 当前 workspace 快照。
    pub fn workspace_path(&self) -> PathBuf {
        self.workspace.read().clone()
    }

    /// 热切换 workspace(与 `AgentConfig::set_workspace` 写同一把锁)。
    pub fn set_workspace(&self, path: PathBuf) {
        *self.workspace.write() = path;
    }

    /// 读取 worktree 会话状态快照。
    pub fn worktree_state(&self) -> Option<SessionWorktreeState> {
        self.worktree.read().clone()
    }

    /// 写入 worktree 会话状态。
    pub fn set_worktree_state(&self, state: Option<SessionWorktreeState>) {
        *self.worktree.write() = state;
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            workspace: Arc::new(RwLock::new(PathBuf::new())),
            worktree: Arc::new(RwLock::new(None)),
            cancel: CancellationToken::new(),
            timeout: Duration::from_secs(120),
            call_id: String::new(),
            session_id: ThreadId::new(),
            turn_id: TurnId::new(),
            env: HashMap::new(),
            permission_mode: PermissionMode::Auto,
            metadata: serde_json::json!({}),
            approval: None,
            session_usage: Arc::new(RwLock::new(reflect_protocol::TokenUsage::default())),
            token_budget: Arc::new(RwLock::new(None)),
            context_window_size: Arc::new(RwLock::new(None)),
            progress: None,
            event_forwarder: None,
            parent_tail_json: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Dummy;
    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "no-op"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _: ToolContext,
            _: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![],
                is_error: false,
                metadata: serde_json::Value::Null,
                elapsed_ms: 0,
            })
        }
    }

    #[test]
    fn default_ctx_has_two_minute_timeout() {
        let c = ToolContext::default();
        assert_eq!(c.timeout, Duration::from_secs(120));
    }

    #[test]
    fn dummy_says_concurrency_safe() {
        assert!(Dummy.is_concurrency_safe());
    }

    #[test]
    fn default_ctx_starts_in_auto_mode() {
        let c = ToolContext::default();
        assert_eq!(c.permission_mode, PermissionMode::Auto);
    }
}
