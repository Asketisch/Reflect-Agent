"""ZhuYe 协议 v0 —— SDK 与 `zhuye serve` 之间的 wire 格式。

协议冻结;Rust 核心新增变体属 non-breaking addition。本模块手工定义
协议类型(代替 pydantic / dataclasses-json 之类运行时依赖),用最小
成本让 SDK 在纯 stdlib 上跑起来。事件类型用 `TypedDict` 描述以便静态
检查与 IDE 自动补全。
"""

from __future__ import annotations

from typing import Any, Literal, TypedDict, Union

# ── 常量 ─────────────────────────────────────────────────────────────────

EVENT_ID_NONE = ""

# 可选字段通过 total=False 基类声明,兼容 Python 3.10 且无需 typing_extensions。

# ── Submission / Op ──────────────────────────────────────────────────────


class TextItem(TypedDict):
    type: Literal["text"]
    text: str


class ImageItem(TypedDict):
    type: Literal["image"]


UserInputItem = Union[TextItem, ImageItem]


class ThreadSettings(TypedDict, total=False):
    effort: Literal["low", "medium", "high"] | None
    permission_mode: str | None
    model: str | None


class _UserInputOpOptionalFields(TypedDict, total=False):
    thread_settings: ThreadSettings


class UserInputOp(_UserInputOpOptionalFields):
    type: Literal["user_input"]
    items: list[UserInputItem]


class CompactOp(TypedDict):
    type: Literal["compact"]


class _InterruptOpOptionalFields(TypedDict, total=False):
    child_id: str | None


class InterruptOp(_InterruptOpOptionalFields):
    type: Literal["interrupt"]


class _RewindOpOptionalFields(TypedDict, total=False):
    to_turn_id: str | None


class RewindOp(_RewindOpOptionalFields):
    type: Literal["rewind"]


class ShutdownOp(TypedDict):
    type: Literal["shutdown"]


class ReviewDecisionDenyReason(TypedDict):
    reason: str


class ReviewDecisionDeny(TypedDict):
    deny: ReviewDecisionDenyReason


ReviewDecisionApprove = Literal["approve", "approve_for_session"]
ReviewDecision = Union[ReviewDecisionApprove, ReviewDecisionDeny]


class ToolApprovalOp(TypedDict):
    type: Literal["tool_approval"]
    id: str
    decision: ReviewDecision


class HookApprovalOp(TypedDict):
    type: Literal["hook_approval"]
    id: str
    decision: ReviewDecision


class EnterPlanModeOp(TypedDict):
    type: Literal["enter_plan_mode"]
    task: str


class ExitPlanModeOp(TypedDict):
    type: Literal["exit_plan_mode"]


class PlanApprovalOp(TypedDict):
    type: Literal["plan_approval"]
    id: str
    choice: Literal["auto_mode", "manual_approve", "revise"]


class SetEffortOp(TypedDict):
    type: Literal["set_effort"]
    effort: Literal["low", "medium", "high"]


class Answer(TypedDict, total=False):
    selected: list[int]
    custom: str | None


class AskUserAnswer(TypedDict):
    answers: list[Answer]


class AskUserQuestionResponseOp(TypedDict):
    type: Literal["ask_user_question_response"]
    id: str
    answers: AskUserAnswer


class AskUserInputResponseOp(TypedDict):
    type: Literal["ask_user_input_response"]
    id: str
    text: str


class SetPermissionModeOp(TypedDict):
    type: Literal["set_permission_mode"]
    mode: Literal["auto", "prompt", "plan", "deny", "bubble"]


class CyclePermissionModeOp(TypedDict):
    type: Literal["cycle_permission_mode"]


class _EnterGoalModeOpOptionalFields(TypedDict, total=False):
    verify_command: str | None
    token_budget: int | None


class EnterGoalModeOp(_EnterGoalModeOpOptionalFields):
    type: Literal["enter_goal_mode"]
    goal: str


class ExitGoalModeOp(TypedDict):
    type: Literal["exit_goal_mode"]


class RemoteToolSpec(TypedDict):
    """v1.3 SDK:客户端自定义工具声明。"""

    name: str
    description: str
    parameters: dict[str, Any]


class RegisterToolsOp(TypedDict):
    type: Literal["register_tools"]
    tools: list[RemoteToolSpec]


class ToolExecutionResponseOp(TypedDict):
    """v1.3 SDK:对 `EventMsg::ToolExecutionRequest` 的回执。"""

    type: Literal["tool_execution_response"]
    call_id: str
    output: dict[str, Any]  # ToolOutput wire dict(见下)


Op = Union[
    UserInputOp,
    CompactOp,
    InterruptOp,
    RewindOp,
    ShutdownOp,
    ToolApprovalOp,
    HookApprovalOp,
    EnterPlanModeOp,
    ExitPlanModeOp,
    PlanApprovalOp,
    SetEffortOp,
    AskUserQuestionResponseOp,
    AskUserInputResponseOp,
    SetPermissionModeOp,
    CyclePermissionModeOp,
    EnterGoalModeOp,
    ExitGoalModeOp,
    RegisterToolsOp,
    ToolExecutionResponseOp,
]


class _SubmissionOptionalFields(TypedDict, total=False):
    client_user_message_id: str


class Submission(_SubmissionOptionalFields):
    id: str
    op: dict[str, Any]  # 宽化为 dict 便于 unknown op 透传


# ── EventMsg ──────────────────────────────────────────────────────────────


class ContentBlock(TypedDict):
    type: Literal["text"]
    text: str


class TokenUsage(TypedDict, total=False):
    input_tokens: int
    output_tokens: int
    cached_tokens: int
    cache_write_tokens: int


class _SessionConfiguredOptionalFields(TypedDict, total=False):
    context_window_size: int | None


class SessionConfigured(_SessionConfiguredOptionalFields):
    type: Literal["session_configured"]
    session_id: str
    model: str
    provider: str
    approval_policy: Literal["auto", "prompt"]
    sandbox_policy: Literal["workspace_only", "full_access"]


class TurnStarted(TypedDict):
    type: Literal["turn_started"]
    turn_id: str
    user_message_id: str


class TurnComplete(TypedDict):
    type: Literal["turn_complete"]
    turn_id: str
    status: Literal["success", "max_iterations", "stopped", "token_budget_exceeded"]
    usage: TokenUsage


class AbortReasonSimple(TypedDict):
    type: Literal["user_interrupt", "shutdown"]


class AbortReasonError(TypedDict):
    type: Literal["error"]
    code: str
    message: str


AbortReason = Union[AbortReasonSimple, AbortReasonError]


class TurnAborted(TypedDict):
    type: Literal["turn_aborted"]
    turn_id: str
    reason: AbortReason


class TurnRewound(TypedDict):
    type: Literal["turn_rewound"]
    to_turn_id: str | None
    truncated_after: int


class ShutdownComplete(TypedDict):
    type: Literal["shutdown_complete"]


class AgentMessage(TypedDict):
    type: Literal["agent_message"]
    text: str


class AgentMessageDelta(TypedDict):
    type: Literal["agent_message_delta"]
    delta: str


class ThinkingDelta(TypedDict):
    type: Literal["thinking_delta"]
    delta: str


class TokenCount(TypedDict, total=False):
    type: Literal["token_count"]
    input_tokens: int
    output_tokens: int
    cached_tokens: int
    cache_write_tokens: int
    total_tokens: int
    provider: str
    credential_label: str


class _ToolCallBeginOptionalFields(TypedDict, total=False):
    child_id: str | None


class ToolCallBegin(_ToolCallBeginOptionalFields):
    type: Literal["tool_call_begin"]
    call_id: str
    tool_name: str
    args: dict[str, Any]


class ToolCallArgsDelta(TypedDict):
    """v1.5:模型生成工具参数期间的原始 JSON 片段(逐段转发)。

    与 `tool_call_begin` 的 `call_id` 对应同一通调用;客户端把各片段按
    到达顺序拼接即得完整参数 JSON。additive 变体 —— 老客户端按未知
    `type` 忽略即可。
    """

    type: Literal["tool_call_args_delta"]
    call_id: str
    delta: str


class ToolCallOutputDelta(TypedDict):
    """工具执行期间的输出预览,最终结果以 tool_call_end 为准。"""

    type: Literal["tool_call_output_delta"]
    call_id: str
    delta: str
    is_stderr: bool


class _ToolCallEndOptionalFields(TypedDict, total=False):
    child_id: str | None


class ToolCallEnd(_ToolCallEndOptionalFields):
    type: Literal["tool_call_end"]
    call_id: str
    output: dict[str, Any]
    is_error: bool
    elapsed_ms: int


class ToolExecutionRequest(TypedDict):
    """v1.3 SDK:请求客户端执行其注册的远程工具。"""

    type: Literal["tool_execution_request"]
    call_id: str
    tool: str
    args: dict[str, Any]


class ToolApprovalKind(TypedDict):
    type: Literal["tool"]
    tool_name: str
    args: dict[str, Any]


class HookApprovalKind(TypedDict):
    type: Literal["hook"]
    hook_name: str
    decision_preview: str


ApprovalKind = Union[ToolApprovalKind, HookApprovalKind]


class ApprovalRequest(TypedDict):
    type: Literal["approval_request"]
    request_id: str
    kind: ApprovalKind
    risk: Literal["low", "medium", "high"]


class QuestionOption(TypedDict, total=False):
    label: str
    description: str
    preview: str


class Question(TypedDict, total=False):
    header: str
    question: str
    options: list[QuestionOption]
    multi_select: bool


class AskUserQuestion(TypedDict):
    type: Literal["ask_user_question"]
    request_id: str
    questions: list[Question]


class _AskUserInputOptionalFields(TypedDict, total=False):
    secret: bool
    placeholder: str | None


class AskUserInput(_AskUserInputOptionalFields):
    type: Literal["ask_user"]
    request_id: str
    prompt: str


class _PermissionBubbleOptionalFields(TypedDict, total=False):
    args_preview: str | None


class PermissionBubble(_PermissionBubbleOptionalFields):
    type: Literal["permission_bubble"]
    tool_name: str
    risk: Literal["low", "medium", "high"]


class _ContextCompactedOptionalFields(TypedDict, total=False):
    before_tokens: int
    after_tokens: int


class ContextCompacted(_ContextCompactedOptionalFields):
    type: Literal["context_compacted"]
    strategy: Literal["truncate", "summarize"]


class _ErrorEventOptionalFields(TypedDict, total=False):
    details: dict[str, Any]


class ErrorEvent(_ErrorEventOptionalFields):
    type: Literal["error"]
    code: str
    message: str


class StreamError(TypedDict):
    type: Literal["stream_error"]
    code: str
    message: str
    retry_in_ms: int


class _ConfigReloadedOptionalFields(TypedDict, total=False):
    sections_changed: list[str]


class ConfigReloaded(_ConfigReloadedOptionalFields):
    type: Literal["config_reloaded"]


class CollabStarted(TypedDict):
    type: Literal["collab_started"]
    discussion_id: str
    topic: str


class _CollabMessageOptionalFields(TypedDict, total=False):
    token_usage: TokenUsage


class CollabMessage(_CollabMessageOptionalFields):
    type: Literal["collab_message"]
    discussion_id: str
    participant: str
    text: str


class _CollabFinishedOptionalFields(TypedDict, total=False):
    result: str | None


class CollabFinished(_CollabFinishedOptionalFields):
    type: Literal["collab_finished"]
    discussion_id: str


class _McpServerStartedOptionalFields(TypedDict, total=False):
    tool_names: list[str]
    transport: str


class McpServerStarted(_McpServerStartedOptionalFields):
    type: Literal["mcp_server_started"]
    server: str
    tool_count: int


class McpServerFailed(TypedDict):
    type: Literal["mcp_server_failed"]
    server: str
    reason: str
    will_retry: bool


class McpToolInvoked(TypedDict):
    type: Literal["mcp_tool_invoked"]
    server: str
    tool: str
    elapsed_ms: int
    is_error: bool


class LspServerStarted(TypedDict):
    type: Literal["lsp_server_started"]
    server: str
    methods: list[str]
    language_ids: list[str]


class LspServerFailed(TypedDict):
    type: Literal["lsp_server_failed"]
    server: str
    reason: str


class PlanRequest(TypedDict):
    type: Literal["plan_request"]
    task: str


class PlanReady(TypedDict):
    type: Literal["plan_ready"]
    plan_id: str
    markdown: str


class PlanApproved(TypedDict):
    type: Literal["plan_approved"]
    plan_id: str
    choice: Literal["auto_mode", "manual_approve", "revise"]


class _PlanRejectedOptionalFields(TypedDict, total=False):
    reason: str | None


class PlanRejected(_PlanRejectedOptionalFields):
    type: Literal["plan_rejected"]
    plan_id: str


class PermissionModeChanged(TypedDict):
    type: Literal["permission_mode_changed"]
    from_: str
    to: str


class PlanDraftUpdated(TypedDict):
    type: Literal["plan_draft_updated"]
    draft_id: str
    markdown: str


class PlanStep(TypedDict):
    type: Literal["plan_step"]
    plan_id: str
    index: int
    total: int
    status: Literal["pending", "in_progress", "completed", "failed"]
    step: str


class PluginLoaded(TypedDict):
    type: Literal["plugin_loaded"]
    plugin: str
    skill_count: int
    command_count: int


class _QuotaExhaustedOptionalFields(TypedDict, total=False):
    cooldown_until: str | None


class QuotaExhausted(_QuotaExhaustedOptionalFields):
    type: Literal["quota_exhausted"]
    credential: str


class _RoutingOptionalFields(TypedDict, total=False):
    detail: str


class Routing(_RoutingOptionalFields):
    type: Literal["routing"]
    role: str
    kind: str


class UnknownEventMsg(TypedDict):
    """协议未识别的 EventMsg 降级形态。"""

    type: str  # type: ignore[misc]


EventMsg = Union[
    SessionConfigured,
    TurnStarted,
    TurnComplete,
    TurnAborted,
    TurnRewound,
    ShutdownComplete,
    AgentMessage,
    AgentMessageDelta,
    ThinkingDelta,
    TokenCount,
    ToolCallBegin,
    ToolCallArgsDelta,
    ToolCallOutputDelta,
    ToolCallEnd,
    ToolExecutionRequest,
    ApprovalRequest,
    AskUserQuestion,
    AskUserInput,
    PermissionBubble,
    ContextCompacted,
    ErrorEvent,
    StreamError,
    ConfigReloaded,
    CollabStarted,
    CollabMessage,
    CollabFinished,
    McpServerStarted,
    McpServerFailed,
    McpToolInvoked,
    LspServerStarted,
    LspServerFailed,
    PlanRequest,
    PlanReady,
    PlanApproved,
    PlanRejected,
    PermissionModeChanged,
    PlanDraftUpdated,
    PlanStep,
    PluginLoaded,
    QuotaExhausted,
    Routing,
    UnknownEventMsg,
]


class Event(TypedDict):
    id: str
    msg: dict[str, Any]


class ToolOutput(TypedDict, total=False):
    """工具回执的 wire 格式(`Op::ToolExecutionResponse.output`)。"""

    content: list[ContentBlock]
    is_error: bool
    metadata: dict[str, Any]
    elapsed_ms: int