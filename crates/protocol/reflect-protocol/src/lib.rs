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
//! reflect-protocol —— Submission / Op / EventMsg / Item 等数据结构定义。
//!
//! 协议是 `reflect-core` 与其客户端(TUI / exec / lib)之间的公共契约。
//! 具备异步友好(mpsc 通道)、可序列化(JSON)以及版本稳定
//! (v0 已冻结,新增变体均以非破坏方式追加)的特性。
//!
//! 完整规范参见 `docs/protocol.md`。

pub mod ask_user_input;
pub mod error;
pub mod event;
pub mod event_msg;
pub mod item;
pub mod op;
pub mod question;
pub mod recorder;
pub mod submission;

pub use error::ProtocolError;
pub use event::{EVENT_ID_NONE, Event};
pub use event_msg::{
    AbortReason, AgentMessage, AgentMessageDelta, ApprovalKind, ApprovalRequestEvent,
    AskUserInputEvent, CollabFinishedEvent, CollabMessageEvent, CollabStartedEvent,
    ConfigReloadedEvent, ContextCompactedEvent, ContextCompactedStrategy, ErrorEvent, EventMsg,
    LspServerFailedEvent, LspServerStartedEvent, McpServerFailedEvent, McpServerStartedEvent,
    McpToolInvokedEvent, McpTransportMirror, PermissionBubbleEvent, PermissionModeChangedEvent,
    PlanApprovedEvent, PlanDraftUpdatedEvent, PlanReadyEvent, PlanRejectedEvent, PlanRequestEvent,
    PlanStepEvent, PlanStepStatus, PluginLoadedEvent, QuotaExhaustedEvent, RoutingEvent,
    RoutingEventKind, StreamErrorEvent, ThinkingDelta, TokenCountEvent, TokenUsage,
    ToolCallBeginEvent, ToolCallEndEvent, ToolCallOutputDeltaEvent, ToolExecutionRequestEvent,
    TriedCredential, TurnAbortedEvent, TurnCompleteEvent, TurnRewoundEvent, TurnStartedEvent,
    TurnStatus,
};
pub use item::{
    ApprovalPolicy, ContentBlock, PermissionMode, PlanApprovalChoice, PlanId,
    ReasoningEffortMirror, ReviewDecision, RiskLevel, SandboxPolicy, SessionConfiguredEvent,
    SteeringPriorityMirror, ThreadId, ThreadSettingsOverrides, ToolError, ToolOutput, TurnId,
    UserInputItem, is_edit_tool_name,
};
pub use op::{Op, RemoteToolSpec};
pub use question::{
    Answer, AskUserAnswer, AskUserQuestionEvent, MAX_HEADER_CHARS, MAX_OPTIONS, MAX_QUESTIONS,
    MIN_OPTIONS, Question, QuestionError, QuestionOption,
};
pub use recorder::{
    MessageRole, NullRecorder, RolloutRecord, RolloutRecorder, SessionInfo, derive_title,
};
pub use submission::{Submission, W3cTraceContext};
