"""Reflect 协议 v0 —— SDK 与 `reflect serve` 之间的 wire 格式。

协议冻结;Rust 核心新增变体属 non-breaking addition。本模块手工定义
协议类型(代替 pydantic / dataclasses-json 之类运行时依赖),用最小
成本让 SDK 在纯 stdlib 上跑起来。事件类型用 `TypedDict` 描述以便静态
检查与 IDE 自动补全。
"""

from __future__ import annotations

from typing import Any, Literal, NotRequired, TypedDict, Union

# ── 常量 ─────────────────────────────────────────────────────────────────

EVENT_ID_NONE = ""

# ── Submission / Op ──────────────────────────────────────────────────────


class TextItem(TypedDict):
    type: Literal["text"]
    text: str


class ImageItem(TypedDict):
    type: Literal["image"]


UserInputItem = Union[TextItem, ImageItem]


class ThreadSettings(TypedDict, total=False):
    effort: NotRequired[Literal["low", "medium", "high"] | None]
    permission_mode: NotRequired[str | None]
    model: NotRequired[str | None]


class UserInputOp(TypedDict):
    type: Literal["user_input"]
    items: list[UserInputItem]
    thread_settings: NotRequired[ThreadSettings]


class CompactOp(TypedDict):
    type: Literal["compact"]


class InterruptOp(TypedDict):
    type: Literal["interrupt"]
    child_id: NotRequired[str | None]


class RewindOp(TypedDict):
    type: Literal["rewind"]
    to_turn_id: NotRequired[str | None]


class ShutdownOp(TypedDict):
    type: Literal["shutdown"]


class ReviewDecisionApprove(TypedDict):
    type: Literal["approve"]


class ReviewDecisionDeny(TypedDict):
    type: Literal["deny"]
    reason: str


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
    choice: Literal["auto_mode", "prompt_mode", "reject"]


class SetEffortOp(TypedDict):
    type: Literal["set_effort"]
    effort: Literal["low", "medium", "high"]


class Answer(TypedDict, total=False):
    selected: list[int]
    custom: NotRequired[str | None]


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


class EnterGoalModeOp(TypedDict):
    type: Literal["enter_goal_mode"]
    goal: str
    verify_command: NotRequired[str | None]
    token_budget: NotRequired[int | None]


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


class Submission(TypedDict):
    id: str
    op: dict[str, Any]  # 宽化为 dict 便于 unknown op 透传
    client_user_message_id: NotRequired[str]


# ── EventMsg ──────────────────────────────────────────────────────────────


class ContentBlock(TypedDict):
    type: Literal["text"]
    text: str


class TokenUsage(TypedDict, total=False):
    input_tokens: int
    output_tokens: int
    cached_tokens: NotRequired[int]
    cache_write_tokens: NotRequired[int]


class SessionConfigured(TypedDict):
    type: Literal["session_configured"]
    session_id: str
    model: str
    provider: str
    approval_policy: Literal["auto", "prompt"]
    sandbox_policy: Literal["workspace_only", "full_access"]
    context_window_size: NotRequired[int | None]


class TurnStarted(TypedDict):
    type: Literal["turn_started"]
    turn_id: str
    user_message_id: str


class TurnComplete(TypedDict):
    type: Literal["turn_complete"]
    turn_id: str
    status: Literal["ok", "cancelled", "aborted", "error"]
    usage: TokenUsage


class TurnAborted(TypedDict):
    type: Literal["turn_aborted"]
    turn_id: str
    reason: str


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
    cached_tokens: NotRequired[int]
    cache_write_tokens: NotRequired[int]
    total_tokens: NotRequired[int]
    provider: NotRequired[str]
    credential_label: NotRequired[str]


class ToolCallBegin(TypedDict):
    type: Literal["tool_call_begin"]
    call_id: str
    tool_name: str
    args: dict[str, Any]
    child_id: NotRequired[str | None]


class ToolCallEnd(TypedDict):
    type: Literal["tool_call_end"]
    call_id: str
    output: dict[str, Any]
    is_error: bool
    elapsed_ms: int
    child_id: NotRequired[str | None]


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
    preview: NotRequired[str]


class Question(TypedDict, total=False):
    header: str
    question: str
    options: list[QuestionOption]
    multi_select: NotRequired[bool]


class AskUserQuestion(TypedDict):
    type: Literal["ask_user_question"]
    request_id: str
    questions: list[Question]


class AskUserInput(TypedDict):
    type: Literal["ask_user"]
    request_id: str
    prompt: str
    secret: NotRequired[bool]
    placeholder: NotRequired[str | None]


class PermissionBubble(TypedDict):
    type: Literal["permission_bubble"]
    tool_name: str
    args_preview: NotRequired[str | None]
    risk: Literal["low", "medium", "high"]


class ContextCompacted(TypedDict):
    type: Literal["context_compacted"]
    strategy: Literal["truncate", "summarize"]
    before_tokens: NotRequired[int]
    after_tokens: NotRequired[int]


class ErrorEvent(TypedDict):
    type: Literal["error"]
    code: str
    message: str
    details: NotRequired[dict[str, Any]]


class StreamError(TypedDict):
    type: Literal["stream_error"]
    code: str
    message: str
    retry_in_ms: int


class ConfigReloaded(TypedDict):
    type: Literal["config_reloaded"]
    sections_changed: NotRequired[list[str]]


class CollabStarted(TypedDict):
    type: Literal["collab_started"]
    discussion_id: str
    topic: str


class CollabMessage(TypedDict):
    type: Literal["collab_message"]
    discussion_id: str
    participant: str
    text: str
    token_usage: NotRequired[TokenUsage]


class CollabFinished(TypedDict):
    type: Literal["collab_finished"]
    discussion_id: str
    result: NotRequired[str | None]


class McpServerStarted(TypedDict):
    type: Literal["mcp_server_started"]
    server: str
    tool_count: int
    tool_names: NotRequired[list[str]]
    transport: NotRequired[str]


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
    choice: Literal["auto_mode", "prompt_mode", "reject"]


class PlanRejected(TypedDict):
    type: Literal["plan_rejected"]
    plan_id: str
    reason: NotRequired[str | None]


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


class QuotaExhausted(TypedDict):
    type: Literal["quota_exhausted"]
    credential: str
    cooldown_until: NotRequired[str | None]


class Routing(TypedDict):
    type: Literal["routing"]
    role: str
    kind: str
    detail: NotRequired[str]


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