//! EventMsg —— core 可 emit 的全部事件的标签联合类型。
//!
//! v0 有 17 个变体。后续新增变体是 additive、非破坏性的(serde 增量化)。
//! M1/M2 暂不提供 `ToolCallOutputDelta`(推迟到 v1)。
//! M6 新增 `ApprovalRequest`(与 `Op::ToolApproval` / `Op::HookApproval` 配对)。
//! M7 新增 `ConfigReloaded`。
//! M10/v0.2.4 新增 `CollabStarted` / `CollabMessage` / `CollabFinished`,
//! 让 discussion 功能对 TUI / headless 消费者可见。

use serde::{Deserialize, Serialize};

pub use crate::ask_user_input::AskUserInputEvent;
use crate::item::{ApprovalPolicy, SandboxPolicy, SessionConfiguredEvent, ThreadId};
pub use crate::question::AskUserQuestionEvent;

mod agent;
mod approval;
mod collab;
mod compaction;
mod config;
mod lsp;
mod mcp;
mod plan;
mod plugin;
mod routing;
mod tool;
mod turn;

pub use agent::{AgentMessage, AgentMessageDelta, ThinkingDelta, TokenCountEvent};
pub use approval::{ApprovalKind, ApprovalRequestEvent, PermissionBubbleEvent};
pub use collab::{CollabFinishedEvent, CollabMessageEvent, CollabStartedEvent};
pub use compaction::{
    ContextCompactedEvent, ContextCompactedStrategy, ErrorEvent, StreamErrorEvent, TriedCredential,
};
pub use config::ConfigReloadedEvent;
pub use lsp::{LspServerFailedEvent, LspServerStartedEvent};
pub use mcp::{
    McpServerFailedEvent, McpServerStartedEvent, McpToolInvokedEvent, McpTransportMirror,
};
pub use plan::{
    PermissionModeChangedEvent, PlanApprovedEvent, PlanDraftUpdatedEvent, PlanReadyEvent,
    PlanRejectedEvent, PlanRequestEvent, PlanStepEvent, PlanStepStatus,
};
pub use plugin::{PluginLoadedEvent, QuotaExhaustedEvent};
pub use routing::{RoutingEvent, RoutingEventKind};
pub use tool::{ToolCallBeginEvent, ToolCallEndEvent, ToolExecutionRequestEvent};
pub use turn::{
    AbortReason, TokenUsage, TurnAbortedEvent, TurnCompleteEvent, TurnRewoundEvent,
    TurnStartedEvent, TurnStatus,
};

/// 用于日志的稳定字符串判别值。
pub type EventMsgDiscriminant = &'static str;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    // 生命周期(6)
    /// 一个线程的首 turn 触发一次。id 字段会是 `EVENT_ID_NONE`。
    SessionConfigured(SessionConfiguredEvent),
    TurnStarted(TurnStartedEvent),
    TurnComplete(TurnCompleteEvent),
    TurnAborted(TurnAbortedEvent),
    /// 批次十九:`Op::Rewind` 成功后发出(TUI 据此裁剪显示)。
    TurnRewound(TurnRewoundEvent),
    /// v1.3 SDK:某条 submission 的 per-turn 通道已排空(该 submission
    /// 处理完毕,不会再有任何事件)。`Event.id` 携带对应 submission id。
    /// serve 在 drain task 结束时发出;非 turn 操作(compact / rewind /
    /// 权限模式切换 / goal 等)没有 `TurnComplete` 之类的终态事件,
    /// SDK 的 `submit_op` 迭代器靠本变体收尾,否则会永久阻塞。
    SubmissionClosed,
    ShutdownComplete,

    // LLM 输出(4)
    /// 非流式 assistant 消息(罕见;M1 emit 的是 delta)。
    AgentMessage(AgentMessage),
    /// 流式 assistant 消息分片。
    AgentMessageDelta(AgentMessageDelta),
    /// Anthropic extended-thinking 分片。
    ThinkingDelta(ThinkingDelta),
    /// Token 用量快照(通常在 turn 结束时 emit)。
    TokenCount(TokenCountEvent),

    // 工具(2;v1 新增 ToolCallOutputDelta)
    ToolCallBegin(ToolCallBeginEvent),
    ToolCallEnd(ToolCallEndEvent),
    /// v1.3 SDK:请求客户端执行其注册的远程自定义工具(实现留在客户端
    /// 进程,core 只做转发与等待)。回执走 `Op::ToolExecutionResponse`。
    ToolExecutionRequest(ToolExecutionRequestEvent),

    // 审批(1;M6)
    /// 工具或 hook 正等待用户审批。客户端应当用与 `request_id` 匹配的
    /// `Op::ToolApproval { id, decision }` 或 `Op::HookApproval { id, decision }`
    /// 回应。
    ApprovalRequest(ApprovalRequestEvent),

    // AskUserQuestion (1; v1.1.0) — LLM 主动发起结构化询问,多题多选项。
    // 客户端通过 `Op::AskUserQuestionResponse { id, answers }` 回执。
    /// LLM 主动向用户发起 1-4 道结构化问题(每题 2-4 选项,可选
    /// multi_select,可填 "Other" 自定义文本)。TUI 收到后弹多题 modal,
    /// 用户按键 → 回执 `Op::AskUserQuestionResponse { id, answers }`,
    /// `ApprovalGate` 的对应 oneshot 收到答案后返回 `AskUserAnswer` 给
    /// `AskUserQuestionTool::execute`,最终进 LLM 消息流。
    AskUserQuestion(AskUserQuestionEvent),

    // ask_user (1; v1.1.0 P1) — LLM 主动发起自由文本询问。
    /// LLM 通过 `ask_user` 工具向用户提问(单行自由文本)。TUI 弹单行
    /// input modal;用户提交 → `Op::AskUserInputResponse { id, text }`。
    AskUserInput(AskUserInputEvent),

    /// Bubble 权限模式下的非阻塞工具执行通知(不等待用户决策)。
    PermissionBubble(PermissionBubbleEvent),

    // 压缩(1)
    ContextCompacted(ContextCompactedEvent),

    // Error (2)
    /// turn 内部可恢复的错误。
    Error(ErrorEvent),
    /// 临时性 LLM 流式错误(core 会重试)。
    StreamError(StreamErrorEvent),

    // 配置(1;M7)
    /// 文件监视器重载了 `~/.reflect/config.toml`;`ModelRegistry` 中的
    /// provider 客户端可能已被替换。UI 可短暂显示一个指示器。
    ConfigReloaded(ConfigReloadedEvent),

    // Routing (1; v1.0) — 多 Provider 路由的 failover / cooldown 事件
    Routing(RoutingEvent),

    // Collab (3; M10 / v0.2.4) — 讨论生命周期,TUI / headless 都可观察
    /// 讨论开始。每次 `DiscussionOrchestrator::run` 在入口、首个
    /// `prompt_for` 步骤之前**恰好 emit 一次**。UI 可显示一个状态徽标。
    CollabStarted(CollabStartedEvent),
    /// 一条 `DiscussionMessage` 已通过总线路由。在 `bus.route` 返回
    /// 之后、调用 `on_event(OrchestratorEvent::AgentTurn)` 的同一条
    /// 代码路径上 emit。`token_usage` 仅在 spawn 线程通过
    /// `SpawnedChild::collect_result_with_usage` 观察到 `TokenCount`
    /// 事件时为 `Some`;旧调用方 emit `None`。
    CollabMessage(CollabMessageEvent),
    /// 讨论结束;镜像已有的 `OrchestratorEvent::Finished`,但在协议层
    /// 暴露,这样 headless 消费者(`reflect exec | jq`)和 TUI 都无需
    /// 订阅 `OrchestratorEvent` 即可响应。
    CollabFinished(CollabFinishedEvent),

    // MCP (3; v0.3) — MCP server 启停 / 调用可见,TUI status_bar / JSONL 推送
    /// 一个 MCP server 握手 + list_tools 成功。`tool_count` 给 TUI 显示用。
    McpServerStarted(McpServerStartedEvent),
    /// 一个 MCP server 启动失败(`spawn` / `initialize` / `list_tools` 任一阶段)。
    /// `will_retry` 为 `true` 表示 HTTP 重连循环还会继续尝试。
    McpServerFailed(McpServerFailedEvent),
    /// 单次 MCP tool call 完成(透传自 LLM tool_call / reflect exec 内部),
    /// 便于 TUI 在 status_bar 高亮"mcp: server.tool"调用链。
    McpToolInvoked(McpToolInvokedEvent),

    // LSP (2; v0.5) — LSP server 启停可见,TUI / JSONL 推送
    /// 一个 LSP server 握手 + initialize 成功。`methods` 字段是从
    /// `ServerCapabilities` 推出来的 method 列表,`language_ids` 是该
    /// server 接管的 LSP languageId 集合(去重)。
    LspServerStarted(LspServerStartedEvent),
    /// 一个 LSP server 启动失败(spawn / initialize 任一阶段)。
    /// `will_retry` 永远为 `false`(LSP 不自动重连,配置错就让用户修)。
    LspServerFailed(LspServerFailedEvent),

    // Plan (5; v1.x) — Plan mode 生命周期,TUI / headless 都可观察
    /// `EnterPlanModeTool` 或 `/plan <task>` slash 触发;core 收到后
    /// 弹出 modal 让用户确认进入 Plan mode。**确认后才**把
    /// `PermissionMode` 切到 `Plan`,而不是立即切换。
    PlanRequest(PlanRequestEvent),
    /// `ExitPlanModeTool` 触发;agent 调研结束,plan markdown 已就绪,
    /// 等用户在 TUI modal 上审批。审批通过后切回 `Prompt` 模式,
    /// 写工具(bash/edit/write)解锁。
    PlanReady(PlanReadyEvent),
    /// 用户在 plan approval modal 上选择 approve。`plan_id` 与
    /// `PlanReady` 的 id 对应,便于前端配对渲染。
    PlanApproved(PlanApprovedEvent),
    /// 用户在 plan approval modal 上选择 reject。`reason` 是可选的用户
    /// 反馈(目前 slash/TUI modal 还没收集具体文本,留 `None`)。
    PlanRejected(PlanRejectedEvent),
    /// `PermissionMode` 状态机切换通知。TUI 收到后立即更新 status bar
    /// 的 `│ plan mode` 黄色 segment,所有订阅方(hook 引擎、tool queue)
    /// 都会读到新 mode。`from` / `to` 都填,便于客户端在日志中追溯。
    PermissionModeChanged(PermissionModeChangedEvent),

    /// v1.x Plan mode 草稿预览:agent 在 Plan mode 下写盘 plan markdown
    /// 后即时 emit,让 TUI 把草稿推到对话流。**不阻塞** agent turn,
    /// 也**不**触发 approval modal —— 真正的 1/2/3 决策仍走 `PlanReady`。
    /// 多次 emit 是覆盖式更新(同 draft_id),让用户能实时看到 plan 演化。
    PlanDraftUpdated(PlanDraftUpdatedEvent),

    /// 批次二十四(#14):单个 plan step 的状态变更(结构化逐步进度)。
    /// agent 在执行 plan 时,每开始 / 完成一个 step 就 emit 一次;
    /// TUI 据此在 task_panel 渲染 checkbox 列表(`[ ]` / `[~]` / `[x]`)。
    /// `plan_id` 配对 `PlanReady`,`index` 从 0 起,`total` 给出 step 总数
    /// (允许 agent 事后追加 → total 可变;TUI 取 max(已知 total, index+1))。
    PlanStep(PlanStepEvent),

    /// 批次二十四(#5):一个插件被 `bootstrap_plugins` 成功扫描 + 注册。
    /// `skill_count` / `command_count` 来自 `LoadedPlugin.skills.len()` /
    /// `commands.len()`,TUI `/plugin` overlay 据此显示 `caps: N skills,
    /// M commands`。镜像 `McpServerStarted` 的 producer→event→reducer 路径。
    PluginLoaded(PluginLoadedEvent),
    /// v1.x 功能 7:某 credential 的 token plan 配额耗尽,已对该 credential
    /// 触发 cooldown 并切到下一个 plan。TUI 据此显示切换提示。
    QuotaExhausted(QuotaExhaustedEvent),
}

impl EventMsg {
    /// 稳定的字符串判别值。
    pub fn discriminant(&self) -> EventMsgDiscriminant {
        match self {
            EventMsg::SessionConfigured(_) => "session_configured",
            EventMsg::TurnStarted(_) => "turn_started",
            EventMsg::TurnComplete(_) => "turn_complete",
            EventMsg::TurnAborted(_) => "turn_aborted",
            EventMsg::TurnRewound(_) => "turn_rewound",
            EventMsg::SubmissionClosed => "submission_closed",
            EventMsg::ShutdownComplete => "shutdown_complete",
            EventMsg::AgentMessage(_) => "agent_message",
            EventMsg::AgentMessageDelta(_) => "agent_message_delta",
            EventMsg::ThinkingDelta(_) => "thinking_delta",
            EventMsg::TokenCount(_) => "token_count",
            EventMsg::ToolCallBegin(_) => "tool_call_begin",
            EventMsg::ToolCallEnd(_) => "tool_call_end",
            EventMsg::ToolExecutionRequest(_) => "tool_execution_request",
            EventMsg::ApprovalRequest(_) => "approval_request",
            EventMsg::AskUserQuestion(_) => "ask_user_question",
            EventMsg::AskUserInput(_) => "ask_user",
            EventMsg::PermissionBubble(_) => "permission_bubble",
            EventMsg::ContextCompacted(_) => "context_compacted",
            EventMsg::Error(_) => "error",
            EventMsg::StreamError(_) => "stream_error",
            EventMsg::ConfigReloaded(_) => "config_reloaded",
            EventMsg::CollabStarted(_) => "collab_started",
            EventMsg::CollabMessage(_) => "collab_message",
            EventMsg::CollabFinished(_) => "collab_finished",
            EventMsg::McpServerStarted(_) => "mcp_server_started",
            EventMsg::McpServerFailed(_) => "mcp_server_failed",
            EventMsg::McpToolInvoked(_) => "mcp_tool_invoked",
            EventMsg::LspServerStarted(_) => "lsp_server_started",
            EventMsg::LspServerFailed(_) => "lsp_server_failed",
            EventMsg::PlanRequest(_) => "plan_request",
            EventMsg::PlanReady(_) => "plan_ready",
            EventMsg::PlanApproved(_) => "plan_approved",
            EventMsg::PlanRejected(_) => "plan_rejected",
            EventMsg::PermissionModeChanged(_) => "permission_mode_changed",
            EventMsg::PlanDraftUpdated(_) => "plan_draft_updated",
            EventMsg::PlanStep(_) => "plan_step",
            EventMsg::PluginLoaded(_) => "plugin_loaded",
            EventMsg::Routing(_) => "routing",
            EventMsg::QuotaExhausted(_) => "quota_exhausted",
        }
    }
}

impl SessionConfiguredEvent {
    pub fn new(model: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            session_id: ThreadId::new(),
            model: model.into(),
            provider: provider.into(),
            approval_policy: ApprovalPolicy::Auto,
            sandbox_policy: SandboxPolicy::WorkspaceOnly,
            context_window_size: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{ContentBlock, PermissionMode, PlanId, RiskLevel, ToolOutput, TurnId};
    use std::path::PathBuf;
    use std::time::SystemTime;

    #[test]
    fn all_variants_serialize_with_type_tag() {
        // 抽查 `type` tag 在 wire 格式上使用 snake_case。
        let e = EventMsg::AgentMessageDelta(AgentMessageDelta { delta: "x".into() });
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains(r#""type":"agent_message_delta""#), "got: {j}");

        let e = EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: TurnId::new(),
            reason: AbortReason::UserInterrupt,
        });
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains(r#""type":"turn_aborted""#), "got: {j}");
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let msg = EventMsg::ToolCallEnd(ToolCallEndEvent {
            call_id: "c1".into(),
            output: ToolOutput {
                content: vec![ContentBlock::text("ok")],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 42,
            },
            is_error: false,
            elapsed_ms: 42,
            child_id: None,
        });
        let j = serde_json::to_string(&msg).unwrap();
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                call_id,
                elapsed_ms,
                ..
            }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(elapsed_ms, 42);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn discriminant_covers_all_variants() {
        let _ = ApprovalPolicy::Auto; // 抑制该符号的 unused 警告
        let all = vec![
            EventMsg::ShutdownComplete,
            EventMsg::SubmissionClosed,
            EventMsg::AgentMessage(AgentMessage {
                text: String::new(),
            }),
            EventMsg::AgentMessageDelta(AgentMessageDelta {
                delta: String::new(),
            }),
            EventMsg::ThinkingDelta(ThinkingDelta {
                delta: String::new(),
                kind: "raw".to_string(),
            }),
            EventMsg::TokenCount(TokenCountEvent {
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 0,
                cost_usd: None,
                ..Default::default()
            }),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: TurnId::new(),
                user_message_id: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: TurnId::new(),
                usage: TokenUsage::default(),
                status: TurnStatus::Success,
            }),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: TurnId::new(),
                reason: AbortReason::UserInterrupt,
            }),
            EventMsg::ContextCompacted(ContextCompactedEvent {
                strategy: ContextCompactedStrategy::Noop,
                removed_messages: 0,
                before_tokens: 0,
                after_tokens: 0,
            }),
            EventMsg::Error(ErrorEvent {
                code: "X".into(),
                message: "y".into(),
                details: None,
            }),
            EventMsg::StreamError(StreamErrorEvent {
                code: "X".into(),
                message: "y".into(),
                retry_in_ms: 0,
                ..Default::default()
            }),
            EventMsg::ToolCallBegin(ToolCallBeginEvent {
                call_id: "c".into(),
                tool_name: "t".into(),
                args: serde_json::Value::Null,
                child_id: None,
            }),
            EventMsg::SessionConfigured(SessionConfiguredEvent::new("m", "p")),
            EventMsg::ApprovalRequest(ApprovalRequestEvent {
                request_id: "r1".into(),
                kind: ApprovalKind::Tool {
                    tool_name: "bash".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                },
                risk: RiskLevel::Medium,
            }),
            EventMsg::AskUserQuestion(AskUserQuestionEvent {
                request_id: "q1".into(),
                questions: vec![crate::question::Question {
                    header: "Lang".into(),
                    question: "Pick a language".into(),
                    options: vec![
                        crate::question::QuestionOption {
                            label: "Rust".into(),
                            description: "safe + fast".into(),
                            preview: None,
                        },
                        crate::question::QuestionOption {
                            label: "Go".into(),
                            description: "simple".into(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
            }),
            EventMsg::ConfigReloaded(ConfigReloadedEvent {
                path: PathBuf::from("/home/u/.reflect/config.toml"),
                sections_changed: vec!["anthropic".into()],
                at: SystemTime::UNIX_EPOCH,
            }),
            EventMsg::CollabStarted(CollabStartedEvent {
                id: "00000000-0000-0000-0000-000000000000".into(),
                participants: vec!["a".into(), "b".into()],
                mode: "sequential".into(),
            }),
            EventMsg::CollabMessage(CollabMessageEvent {
                id: "00000000-0000-0000-0000-000000000000".into(),
                from: "a".into(),
                kind: "utterance".into(),
                content: "hi".into(),
                round: 0,
                token_usage: None,
            }),
            EventMsg::CollabFinished(CollabFinishedEvent {
                id: "00000000-0000-0000-0000-000000000000".into(),
                outcome: "consensus".into(),
                rounds: 2,
            }),
            EventMsg::McpServerStarted(McpServerStartedEvent {
                server: "fs".into(),
                tool_count: 3,
                tool_names: vec![],
                transport: McpTransportMirror::Stdio,
            }),
            EventMsg::McpServerFailed(McpServerFailedEvent {
                server: "fs".into(),
                error: "spawn: not found".into(),
                will_retry: false,
            }),
            EventMsg::McpToolInvoked(McpToolInvokedEvent {
                server: "fs".into(),
                tool: "read_file".into(),
                call_id: "call_abc".into(),
            }),
            EventMsg::LspServerStarted(LspServerStartedEvent {
                server: "rust".into(),
                methods: vec![
                    "textDocument/definition".to_string(),
                    "textDocument/references".to_string(),
                    "textDocument/hover".to_string(),
                ],
                language_ids: vec!["rust".to_string()],
            }),
            EventMsg::LspServerFailed(LspServerFailedEvent {
                server: "rust".into(),
                error: "spawn: executable not found".into(),
                will_retry: false,
            }),
            EventMsg::PlanRequest(PlanRequestEvent {
                plan_id: PlanId::new(),
                task: "refactor auth".into(),
            }),
            EventMsg::PlanReady(PlanReadyEvent {
                plan_id: PlanId::new(),
                markdown: "## Plan\n1. read auth.rs\n2. edit token validation".into(),
                path: None,
            }),
            EventMsg::PlanApproved(PlanApprovedEvent {
                plan_id: PlanId::new(),
            }),
            EventMsg::PlanRejected(PlanRejectedEvent {
                plan_id: PlanId::new(),
                reason: None,
            }),
            EventMsg::PermissionModeChanged(PermissionModeChangedEvent {
                from: PermissionMode::Auto,
                to: PermissionMode::Plan,
            }),
            EventMsg::PlanDraftUpdated(PlanDraftUpdatedEvent {
                draft_id: "refactor.md".into(),
                markdown: "## Draft\n- step 1".into(),
                path: Some(PathBuf::from("/ws/.reflect/plan/refactor.md")),
            }),
            EventMsg::PlanStep(PlanStepEvent {
                plan_id: PlanId::new(),
                index: 1,
                total: 3,
                status: PlanStepStatus::InProgress,
                title: Some("read auth.rs".into()),
            }),
            EventMsg::PluginLoaded(PluginLoadedEvent {
                plugin: "my-plugin".into(),
                scope: "project".into(),
                version: "0.1.0".into(),
                skill_count: 2,
                command_count: 1,
            }),
        ];
        for v in &all {
            assert!(!v.discriminant().is_empty());
        }
        // 验证没有两个变体发生冲突。
        let mut d: Vec<_> = all.iter().map(|v| v.discriminant()).collect();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), all.len());
    }

    #[test]
    fn approval_request_tool_kind_roundtrip() {
        let ev = EventMsg::ApprovalRequest(ApprovalRequestEvent {
            request_id: "req-7".into(),
            kind: ApprovalKind::Tool {
                tool_name: "bash".into(),
                args: serde_json::json!({"cmd": "rm -rf /tmp/x"}),
            },
            risk: RiskLevel::High,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"approval_request""#), "got: {j}");
        assert!(j.contains(r#""risk":"high""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::ApprovalRequest(e) => {
                assert_eq!(e.request_id, "req-7");
                match e.kind {
                    ApprovalKind::Tool { tool_name, args } => {
                        assert_eq!(tool_name, "bash");
                        assert_eq!(args["cmd"], "rm -rf /tmp/x");
                    }
                    ApprovalKind::Hook { .. } => panic!("wrong kind"),
                    ApprovalKind::Plan { .. } => panic!("wrong kind"),
                }
                assert_eq!(e.risk, RiskLevel::High);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn approval_request_hook_kind_roundtrip() {
        let ev = EventMsg::ApprovalRequest(ApprovalRequestEvent {
            request_id: "req-9".into(),
            kind: ApprovalKind::Hook {
                hook_name: "dangerous_command_blocker".into(),
                decision_preview: "deny: matched 'rm -rf /'".into(),
            },
            risk: RiskLevel::default(),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""risk":"low""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        if let EventMsg::ApprovalRequest(e) = back {
            assert!(matches!(e.kind, ApprovalKind::Hook { .. }));
            assert_eq!(e.risk, RiskLevel::Low);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn risk_level_default_is_low() {
        assert_eq!(RiskLevel::default(), RiskLevel::Low);
    }

    #[test]
    fn approval_request_omitted_risk_deserializes_as_low() {
        // Wire 兼容性:省略该字段的 producer 应解析为 Low。
        let j = r#"{"type":"approval_request","request_id":"r","kind":{"type":"tool","tool_name":"t","args":{}}}"#;
        let back: EventMsg = serde_json::from_str(j).unwrap();
        if let EventMsg::ApprovalRequest(e) = back {
            assert_eq!(e.risk, RiskLevel::Low);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn token_count_with_cost_usd_roundtrip() {
        // M8 P1a:`cost_usd` 为 `Option<f64>`;Some 与 None 两个分支都必须能
        // 通过 serde 完成 roundtrip。
        let ev = EventMsg::TokenCount(TokenCountEvent {
            input_tokens: 1000,
            output_tokens: 200,
            cached_tokens: 50,
            cache_write_tokens: 30,
            total_tokens: 1200,
            cost_usd: Some(0.0009),
            ..Default::default()
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""cost_usd":0.0009"#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::TokenCount(e) => {
                assert_eq!(e.cost_usd, Some(0.0009));
                assert_eq!(e.cache_write_tokens, 30);
                assert_eq!(e.cached_tokens, 50);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // None 分支:序列化时必须省略(`skip_serializing_if`)。
        let ev_none = EventMsg::TokenCount(TokenCountEvent {
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
            ..Default::default()
        });
        let j = serde_json::to_string(&ev_none).unwrap();
        assert!(!j.contains("cost_usd"), "None must be skipped, got: {j}");
    }

    #[test]
    fn token_count_backward_compat_with_m7_wire() {
        // M8:M7 producer 没有 `cache_write_tokens` 或 `cost_usd`。
        // M8 consumer 必须仍能接受 M7 的 wire 格式。
        let j = r#"{"type":"token_count","input_tokens":1,"output_tokens":2,"cached_tokens":3,"total_tokens":3}"#;
        let back: EventMsg = serde_json::from_str(j).unwrap();
        match back {
            EventMsg::TokenCount(e) => {
                assert_eq!(e.input_tokens, 1);
                assert_eq!(e.cached_tokens, 3);
                assert_eq!(e.cache_write_tokens, 0, "missing field defaults to 0");
                assert_eq!(e.cost_usd, None, "missing field defaults to None");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn config_reloaded_roundtrip() {
        use std::time::Duration;
        let ev = EventMsg::ConfigReloaded(ConfigReloadedEvent {
            path: PathBuf::from("/home/u/.reflect/config.toml"),
            sections_changed: vec!["anthropic".into(), "compact".into()],
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"config_reloaded""#), "got: {j}");
        assert!(
            j.contains(r#""sections_changed":["anthropic","compact"]"#),
            "got: {j}"
        );
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::ConfigReloaded(e) => {
                assert_eq!(e.path, PathBuf::from("/home/u/.reflect/config.toml"));
                assert_eq!(
                    e.sections_changed,
                    vec!["anthropic".to_string(), "compact".to_string()]
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── M10/v0.2.4:Collab* 事件────────────────────────────────────────

    #[test]
    fn collab_started_roundtrip() {
        let ev = EventMsg::CollabStarted(CollabStartedEvent {
            id: "11111111-2222-3333-4444-555555555555".into(),
            participants: vec!["advocate".into(), "skeptic".into(), "moderator".into()],
            mode: "sequential".into(),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"collab_started""#), "got: {j}");
        assert!(
            j.contains(r#""id":"11111111-2222-3333-4444-555555555555""#),
            "got: {j}"
        );
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::CollabStarted(e) => {
                assert_eq!(e.id, "11111111-2222-3333-4444-555555555555");
                assert_eq!(e.participants.len(), 3);
                assert_eq!(e.mode, "sequential");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn collab_message_with_token_usage_roundtrip() {
        let ev = EventMsg::CollabMessage(CollabMessageEvent {
            id: "abc".into(),
            from: "advocate".into(),
            kind: "utterance".into(),
            content: "I disagree".into(),
            round: 2,
            token_usage: Some(TokenUsage::new(123, 45, 10)),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"collab_message""#), "got: {j}");
        assert!(j.contains(r#""round":2"#), "got: {j}");
        assert!(j.contains(r#""token_usage""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::CollabMessage(e) => {
                assert_eq!(e.id, "abc");
                assert_eq!(e.from, "advocate");
                assert_eq!(e.kind, "utterance");
                assert_eq!(e.round, 2);
                let u = e.token_usage.expect("token_usage roundtrips Some");
                assert_eq!(u.input_tokens, 123);
                assert_eq!(u.output_tokens, 45);
                assert_eq!(u.cached_tokens, 10);
                assert_eq!(u.total_tokens, 168);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn collab_message_without_token_usage_omits_field() {
        let ev = EventMsg::CollabMessage(CollabMessageEvent {
            id: "abc".into(),
            from: "advocate".into(),
            kind: "utterance".into(),
            content: "hi".into(),
            round: 0,
            token_usage: None,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(
            !j.contains("token_usage"),
            "None must be skipped via skip_serializing_if, got: {j}"
        );
        // 反向序列化仍正确还原为 None。
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        if let EventMsg::CollabMessage(e) = back {
            assert!(e.token_usage.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn collab_finished_roundtrip() {
        let ev = EventMsg::CollabFinished(CollabFinishedEvent {
            id: "11111111-2222-3333-4444-555555555555".into(),
            outcome: "consensus".into(),
            rounds: 4,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"collab_finished""#), "got: {j}");
        assert!(j.contains(r#""outcome":"consensus""#), "got: {j}");
        assert!(j.contains(r#""rounds":4"#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::CollabFinished(e) => {
                assert_eq!(e.id, "11111111-2222-3333-4444-555555555555");
                assert_eq!(e.outcome, "consensus");
                assert_eq!(e.rounds, 4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── MCP (v0.3) ───────────────────────────────────────────────────────

    #[test]
    fn mcp_server_started_roundtrip() {
        // stdio 传输。
        let ev = EventMsg::McpServerStarted(McpServerStartedEvent {
            server: "filesystem".into(),
            tool_count: 7,
            tool_names: vec![],
            transport: McpTransportMirror::Stdio,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"mcp_server_started""#), "got: {j}");
        assert!(j.contains(r#""server":"filesystem""#), "got: {j}");
        assert!(j.contains(r#""tool_count":7"#), "got: {j}");
        assert!(j.contains(r#""transport":"stdio""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::McpServerStarted(e) => {
                assert_eq!(e.server, "filesystem");
                assert_eq!(e.tool_count, 7);
                assert_eq!(e.transport, McpTransportMirror::Stdio);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // http transport 也覆盖到。
        let ev_http = EventMsg::McpServerStarted(McpServerStartedEvent {
            server: "notion".into(),
            tool_count: 12,
            tool_names: vec![],
            transport: McpTransportMirror::Http,
        });
        let j_http = serde_json::to_string(&ev_http).unwrap();
        assert!(j_http.contains(r#""transport":"http""#), "got: {j_http}");
    }

    #[test]
    fn mcp_server_failed_roundtrip() {
        let ev = EventMsg::McpServerFailed(McpServerFailedEvent {
            server: "broken".into(),
            error: "spawn: executable not found".into(),
            will_retry: false,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"mcp_server_failed""#), "got: {j}");
        assert!(j.contains(r#""server":"broken""#), "got: {j}");
        assert!(j.contains(r#""will_retry":false"#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::McpServerFailed(e) => {
                assert_eq!(e.server, "broken");
                assert!(e.error.contains("spawn"));
                assert!(!e.will_retry);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // will_retry=true 路径(http 重连循环)也走通。
        let ev_retry = EventMsg::McpServerFailed(McpServerFailedEvent {
            server: "flaky".into(),
            error: "connection reset".into(),
            will_retry: true,
        });
        let j_retry = serde_json::to_string(&ev_retry).unwrap();
        assert!(j_retry.contains(r#""will_retry":true"#), "got: {j_retry}");
    }

    #[test]
    fn mcp_tool_invoked_roundtrip() {
        let ev = EventMsg::McpToolInvoked(McpToolInvokedEvent {
            server: "filesystem".into(),
            tool: "read_file".into(),
            call_id: "toolu_01A".into(),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"mcp_tool_invoked""#), "got: {j}");
        assert!(j.contains(r#""server":"filesystem""#), "got: {j}");
        assert!(j.contains(r#""tool":"read_file""#), "got: {j}");
        assert!(j.contains(r#""call_id":"toolu_01A""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::McpToolInvoked(e) => {
                assert_eq!(e.server, "filesystem");
                assert_eq!(e.tool, "read_file");
                assert_eq!(e.call_id, "toolu_01A");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Plan mode 事件(v1.x)──────────────────────────────────────────

    #[test]
    fn plan_request_event_roundtrip() {
        let ev = EventMsg::PlanRequest(PlanRequestEvent {
            plan_id: PlanId::new(),
            task: "refactor auth module".into(),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"plan_request""#), "got: {j}");
        assert!(j.contains(r#""task":"refactor auth module""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::PlanRequest(e) => assert_eq!(e.task, "refactor auth module"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn plan_ready_event_roundtrip() {
        let pid = PlanId::new();
        let ev = EventMsg::PlanReady(PlanReadyEvent {
            plan_id: pid,
            markdown: "## Plan\n- step 1\n- step 2".into(),
            path: None,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"plan_ready""#), "got: {j}");
        assert!(j.contains(r#""plan_id""#), "got: {j}");
        assert!(j.contains(r#""markdown""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::PlanReady(e) => {
                assert_eq!(e.plan_id, pid);
                assert!(e.markdown.contains("step 1"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn plan_approved_event_roundtrip() {
        let pid = PlanId::new();
        let ev = EventMsg::PlanApproved(PlanApprovedEvent { plan_id: pid });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"plan_approved""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::PlanApproved(e) => assert_eq!(e.plan_id, pid),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn plan_rejected_event_omits_none_reason() {
        // wire 兼容性:reason: None 必须 skip_serializing_if,不能占位
        let pid = PlanId::new();
        let ev_none = EventMsg::PlanRejected(PlanRejectedEvent {
            plan_id: pid,
            reason: None,
        });
        let j_none = serde_json::to_string(&ev_none).unwrap();
        assert!(!j_none.contains("reason"), "None 应跳过,got: {j_none}");

        // Some 路径也走通。
        let ev_some = EventMsg::PlanRejected(PlanRejectedEvent {
            plan_id: pid,
            reason: Some("plan 风险太大".into()),
        });
        let j_some = serde_json::to_string(&ev_some).unwrap();
        assert!(
            j_some.contains(r#""reason":"plan 风险太大""#),
            "got: {j_some}"
        );

        // roundtrip: Some / None 都正确还原。
        let back_some: EventMsg = serde_json::from_str(&j_some).unwrap();
        if let EventMsg::PlanRejected(e) = back_some {
            assert_eq!(e.reason.as_deref(), Some("plan 风险太大"));
        } else {
            panic!("wrong variant");
        }
        let back_none: EventMsg = serde_json::from_str(&j_none).unwrap();
        if let EventMsg::PlanRejected(e) = back_none {
            assert!(e.reason.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn plan_draft_updated_event_roundtrip() {
        // wire 兼容性:path: None 必须 skip_serializing_if,不能占位;
        // Some 路径必须能还原 draft_id / markdown / path 三字段。
        let ev = EventMsg::PlanDraftUpdated(PlanDraftUpdatedEvent {
            draft_id: "refactor.md".into(),
            markdown: "## Plan\n1. read\n2. edit".into(),
            path: Some(PathBuf::from("/ws/.reflect/plan/refactor.md")),
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"plan_draft_updated""#), "got: {j}");
        assert!(j.contains(r#""draft_id":"refactor.md""#), "got: {j}");
        assert!(
            j.contains(r#""path":"/ws/.reflect/plan/refactor.md""#),
            "got: {j}"
        );

        let back: EventMsg = serde_json::from_str(&j).unwrap();
        if let EventMsg::PlanDraftUpdated(e) = back {
            assert_eq!(e.draft_id, "refactor.md");
            assert_eq!(e.markdown, "## Plan\n1. read\n2. edit");
            assert_eq!(
                e.path.as_deref(),
                Some(std::path::Path::new("/ws/.reflect/plan/refactor.md"))
            );
        } else {
            panic!("wrong variant: {back:?}");
        }
    }

    #[test]
    fn plan_draft_updated_event_omits_none_path() {
        // path: None 时不应出现在 wire format(向前兼容)。
        let ev = EventMsg::PlanDraftUpdated(PlanDraftUpdatedEvent {
            draft_id: "x.md".into(),
            markdown: "draft".into(),
            path: None,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(!j.contains(r#""path""#), "path: None 应跳过, got: {j}");

        // 反序列化恢复为 None。
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        if let EventMsg::PlanDraftUpdated(e) = back {
            assert!(e.path.is_none(), "path 应反序列化为 None");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn permission_mode_changed_event_roundtrip() {
        let ev = EventMsg::PermissionModeChanged(PermissionModeChangedEvent {
            from: PermissionMode::Auto,
            to: PermissionMode::Plan,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(
            j.contains(r#""type":"permission_mode_changed""#),
            "got: {j}"
        );
        // PermissionMode::Plan 用 snake_case 序列化为 "plan"。
        assert!(j.contains(r#""to":"plan""#), "got: {j}");
        assert!(j.contains(r#""from":"auto""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::PermissionModeChanged(e) => {
                assert_eq!(e.from, PermissionMode::Auto);
                assert_eq!(e.to, PermissionMode::Plan);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // 退出路径(Plan → Prompt)。
        let back_to_prompt = EventMsg::PermissionModeChanged(PermissionModeChangedEvent {
            from: PermissionMode::Plan,
            to: PermissionMode::Prompt,
        });
        let j_back = serde_json::to_string(&back_to_prompt).unwrap();
        assert!(j_back.contains(r#""from":"plan""#), "got: {j_back}");
        assert!(j_back.contains(r#""to":"prompt""#), "got: {j_back}");
    }

    #[test]
    fn approval_kind_plan_roundtrip() {
        let pid = PlanId::new();
        let ev = EventMsg::ApprovalRequest(ApprovalRequestEvent {
            request_id: "req-plan-1".into(),
            kind: ApprovalKind::Plan {
                plan_id: pid,
                summary: "Refactor auth module: split into 3 files".into(),
            },
            risk: RiskLevel::Medium,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"plan""#), "Plan kind tag: got: {j}");
        assert!(j.contains(r#""plan_id""#), "got: {j}");
        assert!(j.contains(r#""summary""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::ApprovalRequest(e) => match e.kind {
                ApprovalKind::Plan { plan_id, summary } => {
                    assert_eq!(plan_id, pid);
                    assert!(summary.contains("Refactor"));
                }
                _ => panic!("wrong kind"),
            },
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── v1.1.0:AskUserQuestion ──────────────────────────────────────

    #[test]
    fn ask_user_question_event_roundtrip() {
        use crate::question::{Question, QuestionOption};
        let q = Question {
            header: "Lang".into(),
            question: "Pick a language".into(),
            options: vec![
                QuestionOption {
                    label: "Rust".into(),
                    description: "safe + fast".into(),
                    preview: None,
                },
                QuestionOption {
                    label: "Go".into(),
                    description: "simple".into(),
                    preview: Some("```\nfn main() {}\n```".into()),
                },
            ],
            multi_select: false,
        };
        let ev = EventMsg::AskUserQuestion(AskUserQuestionEvent::new("q1", vec![q]));
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"ask_user_question""#), "got: {j}");
        assert!(j.contains(r#""request_id":"q1""#), "got: {j}");
        assert!(j.contains(r#""header":"Lang""#), "got: {j}");
        assert!(j.contains(r#""multi_select":false"#), "got: {j}");

        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::AskUserQuestion(e) => {
                assert_eq!(e.request_id, "q1");
                assert_eq!(e.questions.len(), 1);
                assert_eq!(e.questions[0].header, "Lang");
                assert_eq!(e.questions[0].options.len(), 2);
                assert!(e.questions[0].options[1].preview.is_some());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_multi_question_roundtrip() {
        use crate::question::{Question, QuestionOption};
        let qs = vec![
            Question {
                header: "Lang".into(),
                question: "Pick a language".into(),
                options: vec![
                    QuestionOption {
                        label: "Rust".into(),
                        description: "safe + fast".into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Go".into(),
                        description: "simple".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            },
            Question {
                header: "Deploy".into(),
                question: "Where to deploy?".into(),
                options: vec![
                    QuestionOption {
                        label: "AWS".into(),
                        description: "managed".into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "GCP".into(),
                        description: "managed".into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Self".into(),
                        description: "BYO infra".into(),
                        preview: None,
                    },
                ],
                multi_select: true,
            },
        ];
        let ev = EventMsg::AskUserQuestion(AskUserQuestionEvent::new("multi", qs));
        let j = serde_json::to_string(&ev).unwrap();
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::AskUserQuestion(e) => {
                assert_eq!(e.questions.len(), 2);
                assert!(!e.questions[0].multi_select);
                assert!(e.questions[1].multi_select);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
