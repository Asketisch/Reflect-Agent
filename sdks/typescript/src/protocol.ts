//! Reflect 协议 v0 —— SDK 与 `reflect serve` 子命令之间的 wire 格式。
//!
//! 协议冻结,新增变体属 non-breaking addition;SDK 解析未知事件时
//! 退到 `UnknownEventMsg` 不抛错,让 Rust 核心升级不破坏下游。

/**
 * Submission 是 client → serve 的一条命令。`id` 用于把随之产生的事件
 * (`Event.id`)关联回原始 Submission。
 */
export interface Submission {
  id: string;
  op: Op;
  client_user_message_id?: string;
}

/**
 * Op —— 客户端可向 core 提交的操作。`type` 字段是判别符,与 Rust
 * `#[serde(tag = "type", rename_all = "snake_case")]` 一致。
 */
export type Op =
  | { type: 'user_input'; items: UserInputItem[]; thread_settings?: ThreadSettings }
  | { type: 'compact' }
  | { type: 'interrupt'; child_id?: string | null }
  | { type: 'rewind'; to_turn_id?: string | null }
  | { type: 'shutdown' }
  | { type: 'tool_approval'; id: string; decision: ReviewDecision }
  | { type: 'hook_approval'; id: string; decision: ReviewDecision }
  | { type: 'enter_plan_mode'; task: string }
  | { type: 'exit_plan_mode' }
  | { type: 'plan_approval'; id: string; choice: PlanApprovalChoice }
  | { type: 'set_effort'; effort: ReasoningEffort }
  | {
      type: 'ask_user_question_response';
      id: string;
      answers: AskUserAnswer;
    }
  | { type: 'ask_user_input_response'; id: string; text: string }
  | { type: 'set_permission_mode'; mode: PermissionMode }
  | { type: 'cycle_permission_mode' }
  | { type: 'enter_goal_mode'; goal: string; verify_command?: string | null; token_budget?: number | null }
  | { type: 'exit_goal_mode' }
  /** v1.3 SDK:注册客户端自定义工具(实现留在客户端进程)。 */
  | { type: 'register_tools'; tools: RemoteToolSpec[] }
  /** v1.3 SDK:对 `EventMsg::ToolExecutionRequest` 的回执。 */
  | { type: 'tool_execution_response'; call_id: string; output: ToolOutput };

export interface UserInputItem {
  type: 'text' | 'image';
  text?: string;
  // image 字段依 Rust 协议类型当前不向 SDK 暴露,留待后续协议变体。
}

export interface ThreadSettings {
  effort?: ReasoningEffort | null;
  permission_mode?: PermissionMode | null;
  model?: string | null;
}

export type ReasoningEffort = 'low' | 'medium' | 'high';

export type PermissionMode = 'auto' | 'prompt' | 'plan' | 'deny' | 'bubble';

export type PlanApprovalChoice =
  | 'auto_mode'
  | 'prompt_mode'
  | 'reject';

/**
 * `ReviewDecision` 的 wire 形态(serde snake_case)。
 *
 * 注意是**扁平**结构而非 tagged 对象:`Approve` 序列化为裸字符串
 * `"approve"`,`Deny` 序列化为 `{"deny":{"reason":...}}`。旧版误写成
 * `{type:'approve'}`,服务端无法反序列化,审批会永久挂起(v1.3 修复)。
 */
export type ReviewDecision =
  | 'approve'
  | 'approve_for_session'
  | { deny: { reason: string } };

export interface AskUserAnswer {
  answers: Answer[];
}
export interface Answer {
  selected: number[];
  custom?: string | null;
}

/** v1.3 SDK:客户端自定义工具声明。 */
export interface RemoteToolSpec {
  name: string;
  description: string;
  /** JSON Schema 对象。 */
  parameters: Record<string, unknown>;
}

/** v1.3 SDK:远程工具执行结果(回执 wire 格式)。 */
export interface ToolOutput {
  content: ContentBlock[];
  is_error: boolean;
  metadata?: Record<string, unknown>;
  elapsed_ms?: number;
}

export interface ContentBlock {
  type: 'text';
  text: string;
}

/**
 * Event —— core → client 的状态单元。`id` 与 `Submission.id` 对齐;
 * 不绑定 Submission 时(如 `session_configured` / `shutdown_complete`)
 * 取 `EVENT_ID_NONE`(空字符串)。
 */
export interface Event {
  id: string;
  msg: EventMsg;
}

export const EVENT_ID_NONE = '';

/**
 * 已知 EventMsg 变体的联合。未知变体降为 `UnknownEventMsg`(让 Rust
 * 核心新增变体时下游仍能解析)。
 */
export type EventMsg =
  | SessionConfigured
  | TurnStarted
  | TurnComplete
  | TurnAborted
  | TurnRewound
  | ShutdownComplete
  | SubmissionClosed
  | AgentMessage
  | AgentMessageDelta
  | ThinkingDelta
  | TokenCount
  | ToolCallBegin
  | ToolCallEnd
  | ToolExecutionRequest
  | ApprovalRequest
  | AskUserQuestion
  | AskUserInput
  | PermissionBubble
  | ContextCompacted
  | ErrorEvent
  | StreamError
  | ConfigReloaded
  | CollabStarted
  | CollabMessage
  | CollabFinished
  | McpServerStarted
  | McpServerFailed
  | McpToolInvoked
  | LspServerStarted
  | LspServerFailed
  | PlanRequest
  | PlanReady
  | PlanApproved
  | PlanRejected
  | PermissionModeChanged
  | PlanDraftUpdated
  | PlanStep
  | PluginLoaded
  | QuotaExhausted
  | Routing
  | UnknownEventMsg;

export interface SessionConfigured {
  type: 'session_configured';
  session_id: string;
  model: string;
  provider: string;
  approval_policy: 'auto' | 'prompt';
  sandbox_policy: 'workspace_only' | 'full_access';
  context_window_size?: number | null;
}

export interface TurnStarted {
  type: 'turn_started';
  turn_id: string;
  user_message_id: string;
}

export interface TurnComplete {
  type: 'turn_complete';
  turn_id: string;
  status: 'ok' | 'cancelled' | 'aborted' | 'error';
  usage: TokenUsage;
}

export interface TurnAborted {
  type: 'turn_aborted';
  turn_id: string;
  reason: AbortReason;
}

export type AbortReason =
  | 'user_interrupt'
  | 'cancelled'
  | 'error'
  | 'approval_denied'
  | string;

export interface TurnRewound {
  type: 'turn_rewound';
  to_turn_id: string | null;
  truncated_after: number;
}

export interface ShutdownComplete {
  type: 'shutdown_complete';
}

/**
 * v1.3 SDK:某条 submission 的 per-turn 通道已排空(该 submission 在
 * core 处理完毕,不会再有任何事件)。`Event.id` 携带对应 submission id。
 * serve 在 drain task 结束时发出;非 turn 操作(compact / rewind / 权限
 * 模式切换 / goal 等)没有 `turn_complete` 之类的终态事件,`submit()`
 * 迭代器靠本事件收尾,否则会永久挂起。
 */
export interface SubmissionClosed {
  type: 'submission_closed';
}

export interface AgentMessage {
  type: 'agent_message';
  text: string;
}

export interface AgentMessageDelta {
  type: 'agent_message_delta';
  delta: string;
}

export interface ThinkingDelta {
  type: 'thinking_delta';
  delta: string;
}

export interface TokenCount {
  type: 'token_count';
  input_tokens: number;
  output_tokens: number;
  cached_tokens?: number;
  cache_write_tokens?: number;
  total_tokens?: number;
  provider?: string;
  credential_label?: string;
}

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  cached_tokens?: number;
  cache_write_tokens?: number;
}

export interface ToolCallBegin {
  type: 'tool_call_begin';
  call_id: string;
  tool_name: string;
  args: Record<string, unknown>;
  child_id?: string | null;
}

export interface ToolCallEnd {
  type: 'tool_call_end';
  call_id: string;
  output: ToolOutput;
  is_error: boolean;
  elapsed_ms: number;
  child_id?: string | null;
}

/** v1.3 SDK:请求客户端执行其注册的远程工具。 */
export interface ToolExecutionRequest {
  type: 'tool_execution_request';
  call_id: string;
  tool: string;
  args: Record<string, unknown>;
}

export interface ApprovalRequest {
  type: 'approval_request';
  request_id: string;
  kind: ApprovalKind;
  risk: 'low' | 'medium' | 'high';
}

export type ApprovalKind =
  | { type: 'tool'; tool_name: string; args: Record<string, unknown> }
  | { type: 'hook'; hook_name: string; decision_preview: string };

export interface AskUserQuestion {
  type: 'ask_user_question';
  request_id: string;
  questions: Question[];
}

export interface Question {
  header: string;
  question: string;
  options: QuestionOption[];
  multi_select?: boolean;
}

export interface QuestionOption {
  label: string;
  description: string;
  preview?: string;
}

export interface AskUserInput {
  type: 'ask_user';
  request_id: string;
  prompt: string;
  secret?: boolean;
  placeholder?: string | null;
}

export interface PermissionBubble {
  type: 'permission_bubble';
  tool_name: string;
  args_preview?: string | null;
  risk: 'low' | 'medium' | 'high';
}

export interface ContextCompacted {
  type: 'context_compacted';
  strategy: 'truncate' | 'summarize';
  before_tokens?: number;
  after_tokens?: number;
}

export interface ErrorEvent {
  type: 'error';
  code: string;
  message: string;
  details?: Record<string, unknown>;
}

export interface StreamError {
  type: 'stream_error';
  code: string;
  message: string;
  retry_in_ms: number;
  provider?: string;
  credential_label?: string;
}

export interface ConfigReloaded {
  type: 'config_reloaded';
  sections_changed?: string[];
}

export interface CollabStarted {
  type: 'collab_started';
  discussion_id: string;
  topic: string;
}

export interface CollabMessage {
  type: 'collab_message';
  discussion_id: string;
  participant: string;
  text: string;
  token_usage?: TokenUsage;
}

export interface CollabFinished {
  type: 'collab_finished';
  discussion_id: string;
  result?: string | null;
}

export interface McpServerStarted {
  type: 'mcp_server_started';
  server: string;
  tool_count: number;
  tool_names?: string[];
  transport?: string;
}

export interface McpServerFailed {
  type: 'mcp_server_failed';
  server: string;
  reason: string;
  will_retry: boolean;
}

export interface McpToolInvoked {
  type: 'mcp_tool_invoked';
  server: string;
  tool: string;
  elapsed_ms: number;
  is_error: boolean;
}

export interface LspServerStarted {
  type: 'lsp_server_started';
  server: string;
  methods: string[];
  language_ids: string[];
}

export interface LspServerFailed {
  type: 'lsp_server_failed';
  server: string;
  reason: string;
}

export interface PlanRequest {
  type: 'plan_request';
  task: string;
}

export interface PlanReady {
  type: 'plan_ready';
  plan_id: string;
  markdown: string;
}

export interface PlanApproved {
  type: 'plan_approved';
  plan_id: string;
  choice: PlanApprovalChoice;
}

export interface PlanRejected {
  type: 'plan_rejected';
  plan_id: string;
  reason?: string | null;
}

export interface PermissionModeChanged {
  type: 'permission_mode_changed';
  from: PermissionMode;
  to: PermissionMode;
}

export interface PlanDraftUpdated {
  type: 'plan_draft_updated';
  draft_id: string;
  markdown: string;
}

export interface PlanStep {
  type: 'plan_step';
  plan_id: string;
  index: number;
  total: number;
  status: 'pending' | 'in_progress' | 'completed' | 'failed';
  step: string;
}

export interface PluginLoaded {
  type: 'plugin_loaded';
  plugin: string;
  skill_count: number;
  command_count: number;
}

export interface QuotaExhausted {
  type: 'quota_exhausted';
  credential: string;
  cooldown_until?: string | null;
}

export interface Routing {
  type: 'routing';
  role: string;
  kind: string;
  detail?: string;
}

export interface UnknownEventMsg {
  type: string;
  [k: string]: unknown;
}