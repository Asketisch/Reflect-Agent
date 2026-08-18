//! `ToolExecutionQueue` —— 一批工具调用与 hook 集成的执行队列。
//!
//! 协议说明见 `docs/tools-and-hooks.md §3`。unsafe 工具按提交顺序串行执行;
////! concurrency-safe 工具通过 `futures::future::join_all` 并行执行。
//!
//! 队列需要 `HookEngine`(M2+);可通过 `HookEngine::new()` 构造默认的空操作引擎。
//!
//! M6 新增可选的 `ApprovalGate`:传入 `execute_all_with_gate` 后,`required_permission()`
//! 为 `Prompt` 且用户未 `ApproveForSession` 的工具会走 gate —— 发送
//! `EventMsg::ApprovalRequest` 并等待 oneshot,等待用户 `Op::ToolApproval` 回执。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use reflect_hooks::{HookContext, HookEngine, HookEvent};
use reflect_protocol::{
    ContentBlock, PermissionMode, ReviewDecision, RiskLevel, ToolError, ToolOutput,
};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::warn;

use crate::approval::ApprovalGate;
use crate::builtins::bash::{BashCommandClass, classify_command};
use crate::registry::ToolRegistry;
use crate::sanitize::{Sanitizer, sanitize_output};
use crate::tool::ToolContext;

/// 单次工具调用的请求。由 graph 的 `model_call` 节点生成,由
/// `ToolExecutionQueue::execute_all` 消费。
#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    /// 来自 LLM 的稳定 id(`ChatEvent::ToolUseStart.id`),用于把结果与
    /// 模型请求相关联。
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// 单次工具执行的结果,已完成 hook 派发并按模型需要的格式序列化。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub elapsed_ms: u64,
    pub metadata: Value,
}

impl ToolResult {
    /// 包装为 `ContentBlock::ToolResult`,写入 graph 的 `latest_content` 历史。
    pub fn into_content_block(self) -> ContentBlock {
        ContentBlock::ToolResult {
            call_id: self.call_id,
            output: ToolOutput {
                content: self.content,
                is_error: self.is_error,
                metadata: self.metadata,
                elapsed_ms: self.elapsed_ms,
            },
        }
    }
}

/// `ToolContext` 未设置时的单工具默认执行超时。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// safe 工具的最大并发数默认值。
const DEFAULT_MAX_CONCURRENCY: usize = 5;

/// 切分并派发一批工具调用,遵守 unsafe 串行 / safe 并发的规则。
pub struct ToolExecutionQueue {
    registry: Arc<ToolRegistry>,
    hook_engine: Arc<HookEngine>,
    /// 单次调用的基础 `ToolContext`。每次 `execute_single` 会 clone 它,
    /// 并应用单次调用的覆盖字段(`call_id`、`timeout`)。
    base_ctx: ToolContext,
    semaphore: Arc<Semaphore>,
    /// v1.0.0-rc2:工具输出密钥脱敏器,在 `execute_single` 的 `Ok(Ok(_))`
    /// 分支、`tool.execute(...)` 返回之后、`PostToolUse` 派发之前应用。
    /// `Arc` 包装使克隆廉价 —— bootstrap 阶段在 `reflect-exec` 一次性构造,
    /// 后续 clone 共享同一组编译好的正则。`Sanitizer::disabled()` 等价
    /// 「no-op pass」,无须在 queue 内部做 enabled 判断。
    sanitizer: Arc<Sanitizer>,
    /// 会话级 `permission_mode` 的共享句柄(同 `ApprovalGate` 持有的那份)。
    ///
    /// **为什么需要这个**:`base_ctx.permission_mode` 是值类型快照,在
    /// `AgentThread::new` 时由 `shared_tool_context()` 构造,默认 `Auto`,
    /// 之后**永不刷新**。PreToolUse hook(如 `PlanModeGate`)从 `ToolContext`
    /// 读 `permission_mode`,因此切到 Plan 模式后 hook 仍看到 Auto,只读
    /// gate 形同虚设。这里持有共享句柄,`execute_single` 每次 clone ctx 后
    /// 用句柄当前值覆盖 `ctx.permission_mode`,让 hook 拿到实时模式。
    /// `None`(未注入)时维持旧行为,向后兼容。
    session_permission_mode: Option<Arc<parking_lot::RwLock<PermissionMode>>>,
}

impl ToolExecutionQueue {
    /// 用自定义 `base_ctx`、并发上限与
    /// sanitizer. 默认 sanitizer = `Sanitizer::with_defaults()`(10 个
    /// 默认 pattern 启用)。
    pub fn new(
        registry: Arc<ToolRegistry>,
        hook_engine: Arc<HookEngine>,
        base_ctx: ToolContext,
        max_concurrency: usize,
    ) -> Self {
        let max = if max_concurrency == 0 {
            DEFAULT_MAX_CONCURRENCY
        } else {
            max_concurrency
        };
        Self {
            registry,
            hook_engine,
            base_ctx,
            semaphore: Arc::new(Semaphore::new(max)),
            sanitizer: Arc::new(Sanitizer::with_defaults()),
            session_permission_mode: None,
        }
    }

    /// 便捷构造:使用合理默认值(5 路并发、无 session/turn 元数据、
    /// 启用默认 sanitizer)。
    pub fn with_defaults(
        registry: Arc<ToolRegistry>,
        hook_engine: Arc<HookEngine>,
        base_ctx: ToolContext,
    ) -> Self {
        Self::new(registry, hook_engine, base_ctx, DEFAULT_MAX_CONCURRENCY)
    }

    /// 用自定义 sanitizer 构造 queue (escape hatch,主要给 bootstrap /
    /// 集成测试用)。日常使用 `with_defaults` 即可。
    pub fn with_sanitizer(
        registry: Arc<ToolRegistry>,
        hook_engine: Arc<HookEngine>,
        base_ctx: ToolContext,
        sanitizer: Arc<Sanitizer>,
    ) -> Self {
        let mut q = Self::with_defaults(registry, hook_engine, base_ctx);
        q.sanitizer = sanitizer;
        q
    }

    pub fn hook_engine(&self) -> &Arc<HookEngine> {
        &self.hook_engine
    }

    /// 在 queue 共享的引擎上注册一个 hook。M6 示例 API。
    pub fn register_hook<H: reflect_hooks::Hook + 'static>(&self, hook: H) {
        self.hook_engine.register(hook);
    }

    /// 注入会话级 `permission_mode` 共享句柄,让 `execute_single` 每次 clone
    /// ctx 后用句柄当前值覆盖 `ctx.permission_mode`。
    ///
    /// 见 `ToolExecutionQueue.session_permission_mode` 字段文档:此前
    /// `base_ctx.permission_mode` 是构造时的死值,`PlanModeGate` 等
    /// PreToolUse hook 拿不到运行时切换后的模式。注入后 hook 读到实时模式。
    /// 调用方(typically `AgentThread::new`)传入与 `cfg.permission_mode` /
    /// `ApprovalGate` 同源的句柄。
    pub fn set_session_permission_mode(&mut self, mode: Arc<parking_lot::RwLock<PermissionMode>>) {
        self.session_permission_mode = Some(mode);
    }

    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// 执行一批工具调用。返回结果的顺序与输入顺序一致 —— 即便是并发的
    /// safe 子集,因为 `join_all` 保留参数顺序。
    pub async fn execute_all(&self, calls: Vec<ToolCallRequest>) -> Vec<ToolResult> {
        self.execute_all_with_gate(calls, None).await
    }

    /// M6:用可选 `ApprovalGate` 执行一批调用。`Some` 时,`required_permission() == Prompt`
    /// 的工具走 gate;gate 也可能被 `PreToolUse` hook 的 `HookDecision::Ask`
    /// 触发。`None` 时不请求审批(向后兼容 `execute_all` 与无头 `reflect-exec` 驱动)。
    pub async fn execute_all_with_gate(
        &self,
        calls: Vec<ToolCallRequest>,
        gate: Option<Arc<ApprovalGate>>,
    ) -> Vec<ToolResult> {
        // 按并发安全性切分。需保留原始下标,保证最终结果顺序与输入一致。
        let mut indexed_safe: Vec<(usize, ToolCallRequest)> = Vec::new();
        let mut indexed_unsafe: Vec<(usize, ToolCallRequest)> = Vec::new();
        for (i, c) in calls.into_iter().enumerate() {
            match self.registry.get(&c.name) {
                Some(t) if t.is_concurrency_safe() => indexed_safe.push((i, c)),
                _ => indexed_unsafe.push((i, c)),
            }
        }

        // unsafe:串行,按原始顺序。
        let mut unsafe_results: Vec<(usize, ToolResult)> = Vec::new();
        for (idx, call) in indexed_unsafe {
            let r = self.execute_single(call, gate.as_ref()).await;
            unsafe_results.push((idx, r));
        }

        // safe:通过 join_all 并发执行。
        let safe_futures = indexed_safe.into_iter().map(|(idx, call)| {
            let gate_ref = gate.as_ref();
            async move {
                let r = self.execute_single(call, gate_ref).await;
                (idx, r)
            }
        });
        let safe_results: Vec<(usize, ToolResult)> = join_all(safe_futures).await;

        // 按原始顺序合并。
        let mut all: Vec<(usize, ToolResult)> = unsafe_results;
        all.extend(safe_results);
        all.sort_by_key(|(i, _)| *i);
        all.into_iter().map(|(_, r)| r).collect()
    }

    /// 单次工具调用的执行流程:PreToolUse →(可选 ApprovalGate)→ execute →
    /// PostToolUse / PostToolUseFailure。
    async fn execute_single(
        &self,
        call: ToolCallRequest,
        gate: Option<&Arc<ApprovalGate>>,
    ) -> ToolResult {
        // 限制并发数。
        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => {
                return ToolResult {
                    call_id: call.id,
                    content: vec![ContentBlock::text("queue closed")],
                    is_error: true,
                    elapsed_ms: 0,
                    metadata: serde_json::json!({}),
                };
            }
        };

        let tool = self.registry.get(&call.name);
        let tool = match tool {
            Some(t) => t,
            None => {
                return ToolResult {
                    call_id: call.id,
                    content: vec![ContentBlock::text(format!("tool not found: {}", call.name))],
                    is_error: true,
                    elapsed_ms: 0,
                    metadata: serde_json::json!({"error": "tool_not_found"}),
                };
            }
        };

        // 构造本次调用的上下文。
        let mut ctx = self.base_ctx.clone();
        ctx.call_id = call.id.clone();
        // 用会话级共享句柄刷新 `ctx.permission_mode`。`base_ctx.permission_mode`
        // 是构造时的死值(默认 Auto),否则运行时 `/mode plan` 切换后 PreToolUse
        // hook(如 `PlanModeGate`)仍看到旧值,只读 gate 形同虚设。`None`(未
        // 注入)时维持旧行为,向后兼容(headless exec 不切模式)。
        if let Some(mode_handle) = &self.session_permission_mode {
            ctx.permission_mode = *mode_handle.read();
        }
        // v1.0.0-rc1+:queue 拿到 gate 后,顺手 inject 到 `ToolContext.approval`,
        // 让 tool 内部能主动调 `gate.ask_tool(...)` 触发 per-action approval
        // modal(典型:同 tool 不同 action 走不同权限)。`base_ctx.approval` 为 `None`
        // 时保持 `None` —— backward-compat。
        if ctx.approval.is_none() {
            ctx.approval = gate.cloned();
        }
        let effective_timeout = if ctx.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            ctx.timeout
        };

        // PreToolUse 钩子。
        let hook_ctx = HookContext {
            session_id: ctx.session_id,
            turn_id: ctx.turn_id,
            workspace: ctx.workspace_path(),
            permission_mode: ctx.permission_mode,
        };
        let pre_decision = self
            .hook_engine
            .dispatch(&HookEvent::PreToolUse {
                tool: call.name.clone(),
                args: call.args.clone(),
                ctx: hook_ctx,
            })
            .await;

        // 应用决策。
        let mut effective_args = call.args.clone();
        // hook 发出的 Ask reason;处理完简单分支后,若 gate 存在则走 gate。
        // 由 Ask 分支或包含 Ask 的 Combined 分支设置。
        let mut hook_ask_reason: Option<String> = None;
        match pre_decision {
            reflect_hooks::HookDecision::Allow => {}
            reflect_hooks::HookDecision::Deny { reason } => {
                return ToolResult {
                    call_id: call.id,
                    content: vec![ContentBlock::text(format!("Denied by hook: {reason}"))],
                    is_error: true,
                    elapsed_ms: 0,
                    metadata: serde_json::json!({"hook": "deny"}),
                };
            }
            reflect_hooks::HookDecision::Ask { reason } => {
                hook_ask_reason = Some(reason);
            }
            reflect_hooks::HookDecision::ModifyArgs(new_args) => {
                effective_args = new_args;
            }
            reflect_hooks::HookDecision::PermissionOverride(mode) => {
                ctx.permission_mode = mode;
            }
            reflect_hooks::HookDecision::InjectMessage(m) => {
                // 把 reminder 追加到 metadata,模型下一次调用时能看到
                // (M3 v0:暂存到 metadata;M4 可能会迁到独立的 system-reminder 通道)。
                let prev = std::mem::replace(
                    &mut ctx.metadata,
                    serde_json::Value::Object(Default::default()),
                );
                let mut obj = prev.as_object().cloned().unwrap_or_default();
                obj.insert(
                    "system_reminder".into(),
                    serde_json::Value::String(m.content),
                );
                ctx.metadata = serde_json::Value::Object(obj);
            }
            reflect_hooks::HookDecision::Combined(parts) => {
                // 依次应用每个叶子决策。Deny 仍然优先。
                let merged =
                    reflect_hooks::HookEngine::merge(vec![reflect_hooks::HookDecision::Combined(
                        parts,
                    )]);
                match merged {
                    reflect_hooks::HookDecision::Deny { reason } => {
                        return ToolResult {
                            call_id: call.id,
                            content: vec![ContentBlock::text(format!("Denied by hook: {reason}"))],
                            is_error: true,
                            elapsed_ms: 0,
                            metadata: serde_json::json!({"hook": "deny"}),
                        };
                    }
                    reflect_hooks::HookDecision::Ask { reason } => {
                        hook_ask_reason = Some(reason);
                    }
                    reflect_hooks::HookDecision::ModifyArgs(new_args) => {
                        effective_args = new_args;
                    }
                    reflect_hooks::HookDecision::InjectMessage(m) => {
                        let mut obj = ctx.metadata.as_object().cloned().unwrap_or_default();
                        obj.insert(
                            "system_reminder".into(),
                            serde_json::Value::String(m.content),
                        );
                        ctx.metadata = serde_json::Value::Object(obj);
                    }
                    _ => {}
                }
            }
        }

        // M6:审批 gate。两类触发:
        //   1. 工具本身要求 `Prompt` 权限,且用户未在本会话内放行。
        //   2. `PreToolUse` hook 返回了 `HookDecision::Ask`。
        // 无 gate 时(headless `reflect-exec`),两者都是 no-op ——
        // 工具不经提示直接执行,与 M6 之前的行为一致。
        //
        // v1.0.0-rc1+:多 action tool (`ast` / `lsp`) 走 per-action 路由
        // `tool.action_permission(&effective_args)`,默认 fallback 到
        // `tool.required_permission()`。
        if let Some(gate_ref) = gate {
            let effective_perm = tool.action_permission(&effective_args);
            let bash_class = (call.name == "bash")
                .then(|| {
                    effective_args
                        .get("cmd")
                        .and_then(|v| v.as_str())
                        .map(classify_command)
                })
                .flatten();
            let dangerous_bash = bash_class == Some(BashCommandClass::Dangerous);
            let session_allowed = gate_ref.is_session_allowed(&call.name) && !dangerous_bash;
            // v1.x Plan mode 特例:PlanModeGate 已把 bash 按 `classify_command`
            // 分级 —— Safe 放行、Risky/Dangerous 直接 Deny(走不到这里)。
            // 因此到达本审批阶段的 Plan-mode bash 必然是 Safe 只读命令,
            // 再弹 y/n 工具审批属于二次打扰:LLM 反复重试(现场 ×18),
            // 用户却不知要批什么,Plan 调研被卡死。这里短路——Plan mode
            // 下 Safe bash 免审批直接执行,语义与只读 gate 一致。
            let plan_safe_bash = ctx.permission_mode == PermissionMode::Plan
                && call.name == "bash"
                && bash_class == Some(BashCommandClass::Safe);
            let tool_requires_prompt = (matches!(effective_perm, PermissionMode::Prompt)
                && !session_allowed)
                && !plan_safe_bash;
            let risk = tool_risk_level(&call.name, &effective_args);
            // v1.3 safety baseline (plan §五):the previous "Auto + classify Safe ⇒
            // skip ApprovalGate" 的后门已**删除**。现在每次 bash 调用都
            // 与其它 Prompt 模式工具走同一套风险感知 gate 逻辑 ——
            // `dangerous_bash` 无论何种模式都要求显式审批;Safe bash
            // 至少会把请求送进 gate(resolver 或用户仍可通过配置的
            // Allow 规则隐式自动批准,但启发式不再静默放行)。
            let bash_must_approve =
                call.name == "bash" && bash_class == Some(BashCommandClass::Dangerous);
            if tool_requires_prompt || hook_ask_reason.is_some() || bash_must_approve {
                let decision = if let Some(reason) = hook_ask_reason.clone() {
                    // Hook 级别的审批 —— 通过 `ApprovalKind::Hook` 渲染。
                    gate_ref
                        .ask_hook("pre_tool_use", reason, risk, &ctx.cancel)
                        .await
                } else {
                    gate_ref
                        .ask_tool(&call.name, &effective_args, risk, &ctx.cancel)
                        .await
                };
                match decision {
                    ReviewDecision::Approve => {}
                    ReviewDecision::ApproveForSession => {
                        gate_ref.allow_for_session(&call.name);
                    }
                    ReviewDecision::Deny { reason } => {
                        return ToolResult {
                            call_id: call.id,
                            content: vec![ContentBlock::text(format!("Approval denied: {reason}"))],
                            is_error: true,
                            elapsed_ms: 0,
                            metadata: serde_json::json!({"approval": "denied"}),
                        };
                    }
                }
            }
        }

        // 带超时执行。
        let start = Instant::now();
        let exec = tool.execute(ctx, effective_args);
        let outcome = timeout(effective_timeout, exec).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        match outcome {
            Ok(Ok(mut output)) => {
                let is_err = output.is_error;
                // v1.0.0-rc2:在 `output_clone` 之前做密钥脱敏 —— 这样
                // PostToolUse 钩子(LangfuseTracker / tracing 等)只看到
                // 脱敏后版本,密钥不会泄露到 rollout / tracing span。
                // 错误 / 超时分支合成的 `ContentBlock::text(...)` 不二次
                // 扫描 —— 那是工具错误模板,不携带真实密钥。
                //
                // Review 2026-06-30 P0-3:此分支不看 `output.is_error` —
                // 工具合法返回 `ToolOutput { is_error: true, content: real_data }`
                // (典型:Bash exit 0 但 stdout 自标 error) 仍走 sanitize。
                // 这是有意设计:`is_error` 是工具语义信号,不等于"内容
                // 可信",密钥检测对所有真实 payload 都应当生效。仅当
                // 外层 `Result::Err(_)` 或 timeout 触发 `Ok(Err(_))` /
                // `Err(_)` 分支(行 434-486)时跳过 sanitize —— 那里合成
                // 的错误模板文本不含真实工具数据。
                sanitize_output(&mut output, &self.sanitizer);
                let output_clone = output.clone();
                let _ = self
                    .hook_engine
                    .dispatch(&HookEvent::PostToolUse {
                        tool: call.name.clone(),
                        result: output_clone,
                        elapsed_ms,
                    })
                    .await;
                ToolResult {
                    call_id: call.id,
                    content: output.content,
                    is_error: is_err,
                    elapsed_ms,
                    metadata: output.metadata,
                }
            }
            Ok(Err(e)) => {
                let tool_error = e.clone();
                let output = ToolOutput {
                    content: vec![ContentBlock::text(e.to_string())],
                    is_error: true,
                    metadata: serde_json::json!({"kind": format!("{e:?}")}),
                    elapsed_ms,
                };
                let _ = self
                    .hook_engine
                    .dispatch(&HookEvent::PostToolUseFailure {
                        tool: call.name.clone(),
                        error: tool_error,
                        elapsed_ms,
                    })
                    .await;
                ToolResult {
                    call_id: call.id,
                    content: output.content,
                    is_error: true,
                    elapsed_ms,
                    metadata: output.metadata,
                }
            }
            Err(_elapsed) => {
                warn!(tool = %call.name, "tool execution timed out");
                let tool_error = ToolError::Timeout { elapsed_ms };
                let output = ToolOutput {
                    content: vec![ContentBlock::text(format!(
                        "tool '{}' timed out after {elapsed_ms}ms",
                        call.name
                    ))],
                    is_error: true,
                    metadata: serde_json::json!({"timeout_ms": elapsed_ms}),
                    elapsed_ms,
                };
                let _ = self
                    .hook_engine
                    .dispatch(&HookEvent::PostToolUseFailure {
                        tool: call.name.clone(),
                        error: tool_error,
                        elapsed_ms,
                    })
                    .await;
                ToolResult {
                    call_id: call.id,
                    content: output.content,
                    is_error: true,
                    elapsed_ms,
                    metadata: output.metadata,
                }
            }
        }
    }
}

/// 按工具名与参数解析审批风险等级;bash 走 [`classify_command`] 动态映射。
fn tool_risk_level(tool_name: &str, args: &Value) -> RiskLevel {
    if tool_name == "bash" {
        if let Some(cmd) = args.get("cmd").and_then(|v| v.as_str()) {
            return classify_command(cmd).risk_level();
        }
    }
    RiskLevel::Medium
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
    use async_trait::async_trait;
    use reflect_hooks::HookEngine;
    #[allow(unused_imports)] // pre-M5
    use reflect_protocol::{PermissionMode, ThreadId};
    use std::sync::Arc;

    struct StubTool {
        name: String,
        safe: bool,
    }
    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            self.safe
        }
        async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: vec![ContentBlock::text(format!("from {}", self.name))],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 0,
            })
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::for_workspace(".")
    }

    #[tokio::test]
    async fn empty_batch_returns_empty() {
        let reg = Arc::new(ToolRegistry::default());
        let engine = Arc::new(HookEngine::new());
        let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());
        let out = q.execute_all(vec![]).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let reg = Arc::new(ToolRegistry::default());
        let engine = Arc::new(HookEngine::new());
        let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());
        let out = q
            .execute_all(vec![ToolCallRequest {
                id: "1".into(),
                name: "missing".into(),
                args: serde_json::json!({}),
            }])
            .await;
        assert_eq!(out.len(), 1);
        assert!(out[0].is_error);
        assert!(
            out[0]
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("not found")))
        );
    }

    #[tokio::test]
    async fn single_tool_runs_and_returns_result() {
        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(StubTool {
            name: "stub".into(),
            safe: true,
        }));
        let engine = Arc::new(HookEngine::new());
        let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());
        let out = q
            .execute_all(vec![ToolCallRequest {
                id: "1".into(),
                name: "stub".into(),
                args: serde_json::json!({}),
            }])
            .await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_error);
        assert!(matches!(&out[0].content[0], ContentBlock::Text { text } if text == "from stub"));
    }

    #[tokio::test]
    async fn results_preserve_original_order() {
        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(StubTool {
            name: "a".into(),
            safe: true,
        }));
        reg.register(Arc::new(StubTool {
            name: "b".into(),
            safe: true,
        }));
        let engine = Arc::new(HookEngine::new());
        let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());
        let out = q
            .execute_all(vec![
                ToolCallRequest {
                    id: "1".into(),
                    name: "a".into(),
                    args: serde_json::json!({}),
                },
                ToolCallRequest {
                    id: "2".into(),
                    name: "b".into(),
                    args: serde_json::json!({}),
                },
            ])
            .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].call_id, "1");
        assert_eq!(out[1].call_id, "2");
    }

    /// v1.0.0-rc2:StubTool 输出含假 AWS key,经过 queue 走完后应是脱敏后版本。
    #[tokio::test]
    async fn test_sanitize_in_queue_path() {
        use crate::sanitize::Sanitizer;
        use reflect_protocol::ToolOutput;

        struct LeakyStub;
        #[async_trait]
        impl Tool for LeakyStub {
            fn name(&self) -> &str {
                "leaky"
            }
            fn description(&self) -> &str {
                "emits a fake AWS access key"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _ctx: ToolContext,
                _args: Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    content: vec![ContentBlock::text("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE")],
                    is_error: false,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                })
            }
        }

        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(LeakyStub));
        let engine = Arc::new(HookEngine::new());
        let q = ToolExecutionQueue::with_sanitizer(
            reg,
            engine,
            ctx(),
            Arc::new(Sanitizer::with_defaults()),
        );
        let out = q
            .execute_all(vec![ToolCallRequest {
                id: "1".into(),
                name: "leaky".into(),
                args: serde_json::json!({}),
            }])
            .await;
        assert_eq!(out.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text { text } => {
                // KEY_ASSIGN 优先 AWS pattern(因 AWS 在 KEY_ASSIGN 之前),
                // AWS_ACCESS_KEY 已把 AKIAIOSFODNN7EXAMPLE 整段替换为
                // `[REDACTED:aws_key]`。
                assert!(
                    text.contains("[REDACTED:aws_key]"),
                    "expected AWS marker in: {text}"
                );
                assert!(
                    !text.contains("AKIAIOSFODNN7EXAMPLE"),
                    "raw AWS key must not appear: {text}"
                );
            }
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    /// v1.3 safety baseline(plan §五):即使 gate 遇到的 `bash` 工具其命令被
    /// `classify_command` 判定为 `Safe`,queue 也必须向用户呈现 `ApprovalRequest`。
    /// 旧版 `skip_bash_auto_safe` 后门已删除:每条 bash 调用都和 Prompt
    /// 工具一样经过 gate。本测试用 stub gate 端到端验证:
    ///   - stub bash 工具声明 `Prompt` 权限
    ///   - `effective_perm = Prompt` ⇒ `tool_requires_prompt = true`
    ///     → 等待 gate.ask_tool,测试发送 `Approve` 应答
    ///   - 没有 queue 的 gate 询问时,旧路径在 Auto + Safe 下可跳过 modal;
    ///     引入 gate 后,本测试断言请求仍然呈现。
    #[tokio::test]
    async fn bash_safe_classification_does_not_skip_gate() {
        use crate::approval::ApprovalGate;
        use crate::sanitize::Sanitizer;
        use parking_lot::Mutex;
        use std::sync::Arc;

        struct BashStub;
        #[async_trait]
        impl Tool for BashStub {
            fn name(&self) -> &str {
                "bash"
            }
            fn description(&self) -> &str {
                "stub bash"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                false
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Prompt
            }
            async fn execute(
                &self,
                _ctx: ToolContext,
                _args: Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    content: vec![ContentBlock::text("ran")],
                    is_error: false,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                })
            }
        }

        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(BashStub));
        let engine = Arc::new(HookEngine::new());
        let q =
            ToolExecutionQueue::with_sanitizer(reg, engine, ctx(), Arc::new(Sanitizer::disabled()));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(4);
        let waiters: Arc<
            Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<ReviewDecision>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let session_allow: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let gate = ApprovalGate::with_state(
            tx,
            "sub-test",
            waiters.clone(),
            session_allow,
            None,
            None,
            None,
            None,
        );
        let gate_arc = Arc::new(gate);
        // `echo hello` 按 `classify_command` 归类为 `Safe`。但 gate 仍必须被询问:
        // bash 工具声明 `Prompt`,queue 也不再仅凭分类跳过审批。
        let call = ToolCallRequest {
            id: "1".into(),
            name: "bash".into(),
            args: serde_json::json!({"cmd": "echo hello"}),
        };
        // 收到 ApprovalRequest 后,必须在 consumer task 内立即批准,
        // 才能解除 `execute_all_with_gate` 内 `ask_tool` 的 oneshot 等待。
        // 否则 `complete` 排在 `execute_all_with_gate(...).await` 之后,
        // 而后者又依赖 approval 回执 —— 两者互等,形成顺序死锁
        // (单线程 current_thread runtime 下直接挂死,直到 SIGTERM)。
        let gate_for_reply = gate_arc.clone();
        let consumer = tokio::spawn(async move {
            let ev = rx.recv().await.expect("gate event");
            let req_id = match ev.msg {
                reflect_protocol::EventMsg::ApprovalRequest(req) => req.request_id,
                other => panic!("expected ApprovalRequest, got {other:?}"),
            };
            assert!(
                gate_for_reply.complete(&req_id, ReviewDecision::Approve),
                "approval waiter should be found"
            );
        });
        let out = q
            .execute_all_with_gate(vec![call], Some(gate_arc.clone()))
            .await;
        consumer.await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_error, "approved bash should run");
    }

    /// 回归:`ToolExecutionQueue.session_permission_mode` 注入后,PreToolUse
    /// hook(`PlanModeGate`)能读到运行时切换后的实时模式 —— 切到 Plan 后
    /// 写工具应被 hook **Deny**(返回 "Denied by hook"),而不是漏到审批门
    /// 弹 modal。
    ///
    /// 修复前:`base_ctx.permission_mode` 是构造时的死值(默认 Auto),
    /// `execute_single` 只 clone 它,`PlanModeGate` 永远看到 Auto ≠ Plan →
    /// Allow,只读 gate 形同虚设。
    #[tokio::test]
    async fn plan_mode_gate_denies_write_tool_after_mode_injection() {
        use crate::sanitize::Sanitizer;
        use parking_lot::{Mutex, RwLock};

        struct WriteStub;
        #[async_trait]
        impl Tool for WriteStub {
            fn name(&self) -> &str {
                "write"
            }
            fn description(&self) -> &str {
                "stub write"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                false
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Prompt
            }
            async fn execute(
                &self,
                _ctx: ToolContext,
                _args: Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    content: vec![ContentBlock::text("wrote")],
                    is_error: false,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                })
            }
        }

        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(WriteStub));
        let engine = Arc::new(HookEngine::new());
        engine.register(reflect_hooks::builtins::PlanModeGate::default_mode());
        let mut q =
            ToolExecutionQueue::with_sanitizer(reg, engine, ctx(), Arc::new(Sanitizer::disabled()));
        // 注入会话级 mode = Plan(模拟 /mode plan 切换后的状态)。
        q.set_session_permission_mode(Arc::new(RwLock::new(PermissionMode::Plan)));

        // 即便给一个审批门,write 也不该走到它 —— hook 先 Deny。
        let (tx, mut rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(4);
        let waiters: Arc<
            Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<ReviewDecision>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let session_allow: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let gate = ApprovalGate::with_state(
            tx,
            "sub-plan-test",
            waiters,
            session_allow,
            None,
            None,
            None,
            None,
        );
        let gate_arc = Arc::new(gate);

        let call = ToolCallRequest {
            id: "1".into(),
            name: "write".into(),
            args: serde_json::json!({"path": "/tmp/x", "content": "hi"}),
        };
        let out = q
            .execute_all_with_gate(vec![call], Some(gate_arc.clone()))
            .await;

        // hook 应直接 Deny:返回一条 error + "Denied by hook"。
        assert_eq!(out.len(), 1, "write 应有一条结果");
        assert!(
            out[0].is_error,
            "Plan 模式下 write 应被 hook Deny(is_error=true),实际: {:?}",
            out[0].content
        );
        let text: String = out[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text.contains("Denied by hook"),
            "应由 hook Deny,实际: {text}"
        );
        assert!(
            text.contains("Plan mode"),
            "deny reason 应提 Plan mode: {text}"
        );

        // 审批门不应被触发(没发 ApprovalRequest)。
        assert!(
            rx.try_recv().is_err(),
            "hook 已 Deny,审批门不该再发 ApprovalRequest"
        );
    }

    /// 回归:Plan mode 下 Safe bash 免审批短路。
    ///
    /// 现场(用户报障):Plan mode 中 LLM 反复跑只用读命令的 bash,
    /// 每次都被工具审批 `[y/n]` 卡住(bash 声明 `required_permission =
    /// Prompt`),用户不知要批什么,LLM 重试 ×18 也到不了 PlanWrite。
    /// PlanModeGate 已把 bash 分级:Safe 放行、Risky/Dangerous Deny,
    /// 因此到达审批阶段的 Plan-mode bash 必然是 Safe 只读命令 ——
    /// queue 应短路,不弹 y/n,直接执行。
    #[tokio::test]
    async fn plan_mode_safe_bash_skips_approval_gate() {
        use crate::approval::ApprovalGate;
        use crate::sanitize::Sanitizer;
        use parking_lot::{Mutex, RwLock};

        struct BashStub;
        #[async_trait]
        impl Tool for BashStub {
            fn name(&self) -> &str {
                "bash"
            }
            fn description(&self) -> &str {
                "stub bash"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                false
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Prompt
            }
            async fn execute(
                &self,
                _ctx: ToolContext,
                _args: Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    content: vec![ContentBlock::text("ran")],
                    is_error: false,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                })
            }
        }

        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(BashStub));
        let engine = Arc::new(HookEngine::new());
        engine.register(reflect_hooks::builtins::PlanModeGate::default_mode());
        let mut q =
            ToolExecutionQueue::with_sanitizer(reg, engine, ctx(), Arc::new(Sanitizer::disabled()));
        // 注入会话级 mode = Plan(模拟 /mode plan 切换后的状态)。
        q.set_session_permission_mode(Arc::new(RwLock::new(PermissionMode::Plan)));

        let (tx, mut rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(4);
        let waiters: Arc<
            Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<ReviewDecision>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let session_allow: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let gate = ApprovalGate::with_state(
            tx,
            "sub-plan-bash",
            waiters,
            session_allow,
            None,
            None,
            None,
            None,
        );
        let gate_arc = Arc::new(gate);

        // `echo hello` 是 Safe:`PlanModeGate` 放行,`plan_safe_bash`
        // 短路 → 不弹审批,直接执行。
        let call = ToolCallRequest {
            id: "1".into(),
            name: "bash".into(),
            args: serde_json::json!({"cmd": "echo hello"}),
        };
        let out = q
            .execute_all_with_gate(vec![call], Some(gate_arc.clone()))
            .await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_error, "Plan mode Safe bash 应直接执行");
        // 审批门不应被触发(没发 ApprovalRequest)。
        assert!(rx.try_recv().is_err(), "Plan mode Safe bash 不该弹审批");
    }
}
