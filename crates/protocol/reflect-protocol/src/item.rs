//! 由 Submission / Op / EventMsg 引用到的子类型。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::question::AskUserAnswer;

/// 围绕 UUID 的 newtype,用于线程标识。在一个会话的整个生命周期内保持稳定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(pub Uuid);

impl ThreadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 批次二十四(#9):parse UUID 字符串(来自 session overlay 的 `SessionLine.id`)
    /// 为 `ThreadId`,让 `/session` Enter 能解析选中行恢复目标。
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 围绕 UUID 的 newtype,用于回合标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 把 UUID 字符串(例如 `Op::Rewind.to_turn_id` 中的)解析为 `TurnId`。
    /// 供 `submission_loop` 中 rewind-truncate 路径使用,把 wire 形态的
    /// `Option<String>` 转换为 recorder 期望的类型化 id。
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 一段用户输入(文本 / 图片 / 技能激活 / 问题回答)。
///
/// v1.1.0 P1 #14 新增 `QuestionAnswer` —— 允许用户通过标准 `Op::UserInput`
/// 流回答 `EventMsg::AskUserQuestion`(与 `Op::AskUserQuestionResponse` 平行,
/// 适合 TUI 在 question modal 之外用 input bar 自由输入答案的场景)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserInputItem {
    Text {
        text: String,
    },
    Image {
        data: Vec<u8>,
        mime_type: String,
    },
    LocalImage {
        path: PathBuf,
    },
    Skill {
        name: String,
        #[serde(default)]
        args: Option<serde_json::Value>,
    },
    /// v1.1.0 P1 #14: 对 LLM 主动询问的结构化回答。`request_id` 与
    /// `EventMsg::AskUserQuestion.request_id` 配对。`None` 表示用户按
    /// Esc 取消(LLM 收到空答案)。
    QuestionAnswer {
        request_id: String,
        answers: AskUserAnswer,
    },
}

/// 工具运行所需的用户信任级别。
///
/// 放在 `reflect-protocol` 中,用于打破 `reflect-tools` (需要在 `ToolSpec::required_permission` 里使用)
/// 与 `reflect-hooks` (需要在 `HookDecision::PermissionOverride` 里使用) 之间的循环依赖。
/// `reflect-hooks` 重新导出此类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// 工具直接运行,无需提示(只读工具的默认值)。
    #[default]
    Auto,
    /// 工具需经用户审批才能运行(有副作用工具的默认值)。
    Prompt,
    /// 工具被禁止运行。
    Deny,
    /// Plan mode(v1.x):用户主动进入只读规划阶段,
    /// 任何不在白名单内的工具(包括 bash/edit/write)都会被
    /// `PlanModeGate` hook blanket-deny。
    Plan,
    /// 自动批准文件编辑类工具(`write` / `edit` / `delete`),其余
    /// `Prompt` 工具仍走 approval modal(编辑类工具自动批准)。
    AcceptEdits,
    /// 非阻塞 bubble 通知 + 自动批准:emit `PermissionBubble` 事件供
    /// TUI 展示,不弹 blocking modal。
    Bubble,
    /// 危险:静默跳过所有工具审批,不弹 modal、不发 bubble。
    /// `ask_user_question` 等主动索取人类输入的工具(那些仍会弹出)。
    Bypass,
}

impl PermissionMode {
    /// 人类可读标签(用于日志和 TUI approval modal)。
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Prompt => "prompt",
            PermissionMode::Deny => "deny",
            PermissionMode::Plan => "plan",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::Bubble => "bubble",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// TUI `/mode` / Tab / Shift+Tab 的循环顺序。
    /// `default → accept edits → plan → bypass permissions`。
    /// 循环 = `Auto→AcceptEdits→Plan→Bypass→Auto`。
    /// `Prompt | Deny | Bubble` 不进默认循环:从它们循环会落到 `Auto`
    /// (三者仍经 `/mode <name>` 直达,见 `slash::parse_mode`)。
    pub fn next_in_ui_cycle(self) -> Self {
        match self {
            PermissionMode::Auto => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Auto,
            PermissionMode::Bypass => PermissionMode::Auto,
            PermissionMode::Prompt | PermissionMode::Deny | PermissionMode::Bubble => {
                PermissionMode::Auto
            }
        }
    }

    /// 是否应在 `ApprovalGate::ask_tool` 中自动批准(不弹 modal)。
    pub fn auto_approves_tool(&self, tool_name: &str) -> bool {
        match self {
            PermissionMode::Bubble => true,
            PermissionMode::Bypass => false,
            PermissionMode::AcceptEdits => is_edit_tool_name(tool_name),
            _ => false,
        }
    }
}

/// 文件编辑类工具名 —— `AcceptEdits` 模式短路用。
pub fn is_edit_tool_name(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "write" | "edit" | "delete" | "Write" | "Edit" | "Delete"
    )
}

/// Plan 模式的会话级唯一标识。Plan 由 `EnterPlanModeTool` 进入、
/// `ExitPlanModeTool` 退出,整个生命周期由 `plan_id` 串联。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub Uuid);

impl PlanId {
    /// 生成新的 plan 标识(UUID v4)。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for PlanId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(PlanId(Uuid::parse_str(s)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Deny { reason: String },
    ApproveForSession,
}

/// v1.x Plan mode 审批选择(对齐业界通用的三选项体验)。
///
/// `Op::PlanApproval` 携带此枚举,submission_loop 据此决定 plan 批准后的目标
/// `PermissionMode`(而非旧的硬编码 `Prompt`)。与 [`ReviewDecision`] 分开:
/// 后者用于工具/Hook 审批(y/n/a),语义不同(ApproveForSession 在 plan 路径
/// 与 Approve 合并),混用会污染工具审批语义。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanApprovalChoice {
    /// 「Yes, use auto mode」:切到 `AcceptEdits`(自动批准编辑/写入类,
    /// Bash 等仍走白名单/审批)。保守的"自动"——对齐业界通用的 auto mode。
    AutoMode,
    /// 「Yes, manually approve edits」:切到 `Prompt`(逐工具审批,旧行为)。
    ManualApprove,
    /// 「Tell the agent what to change」:留在 plan 模式,用户输入反馈继续 plan。
    /// 不改 permission mode,emit `PlanRejected`(等价于 reject + 回到 plan 编辑)。
    Revise,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSettingsOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_concurrency: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Auto,
    Prompt,
    Deny,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPolicy {
    #[default]
    WorkspaceOnly,
    /// M3+ (requires landlock/mac-sandbox).
    OsSandbox,
    /// M3+ (no restrictions).
    FullAccess,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
}

/// v1.x S4:`/effort` slash 命令的协议镜像枚举。
///
/// 之所以**不**直接复用 `reflect_llm::ReasoningEffort`,是因为
/// `reflect-protocol` 不能反向依赖 `reflect-llm`(避免循环依赖 + 协议层
/// 与实现层解耦)。本枚举与 `reflect_llm::ReasoningEffort` 字段一一对应,
/// 由 `reflect-core::submission_loop` 在收到 `Op::SetEffort` 后调
/// `From<ReasoningEffortMirror> for ReasoningEffort` 桥接到 LLM 路径。
///
/// `Default = Low` 与 Anthropic / OpenAI 默认 reasoning 强度一致;`/effort`
/// 不带参数时也按 Low 处理,避免 "未设置 = 不思考" 的歧义。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortMirror {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfiguredEvent {
    pub session_id: ThreadId,
    pub model: String,
    pub provider: String,
    pub approval_policy: ApprovalPolicy,
    pub sandbox_policy: SandboxPolicy,
    /// 引擎报告的模型上下文窗口大小(token),供 TUI 上下文用量条做分母。
    /// 缺省 `None`(未知模型 / 旧 payload),TUI 此时优雅省略上下文条。
    /// 经 `reflect_llm::context_window_for` 回退表补全,避免与 LLM 层漂移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u32>,
}

/// 工具产生的输出内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub metadata: serde_json::Value,
    pub elapsed_ms: u64,
}

/// 工具执行错误。放在 protocol(而非 `reflect-tools`)中,以便
/// `reflect-hooks` 能在 `HookEvent::PostToolUseFailure` 中携带它,
/// 避免出现 `tools ↔ hooks` 循环依赖。
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolError {
    #[error("invalid arguments: {message}")]
    InvalidArgs { message: String },
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("path escaped sandbox: {path}")]
    PathEscape { path: std::path::PathBuf },
    #[error("io: {0}")]
    Io(String),
    #[error("hook denied: {reason}")]
    HookDenied { reason: String },
    /// v1.3:OS 沙箱不可用且安全基线要求严格模式,fail-closed。
    /// BashTool 不会回退到裸 `sh -c`;直接返回此错误。
    #[error("sandbox unavailable: {reason}")]
    SandboxUnavailable { reason: String },
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Io(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: Vec<u8>,
        mime_type: String,
    },
    Diff {
        unified_diff: String,
    },
    /// LLM 请求的工具调用(M2+)。在模型调用后通过 `latest_content`
    /// 暴露,供 graph 决定是否 dispatch。
    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// 工具执行结果(M2+)。在队列运行后追加到 `latest_content`,
    /// 使下一次模型调用能看到工具输出。
    ToolResult {
        call_id: String,
        output: ToolOutput,
    },
}

impl ContentBlock {
    /// 最常见变体的便捷构造函数。
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_is_unique() {
        let a = ThreadId::new();
        let b = ThreadId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn user_input_item_text_serde() {
        let item = UserInputItem::Text { text: "hi".into() };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains(r#""type":"text""#), "got: {j}");
    }

    #[test]
    fn user_input_item_question_answer_roundtrip() {
        use crate::question::{Answer, AskUserAnswer};
        let item = UserInputItem::QuestionAnswer {
            request_id: "q-1".into(),
            answers: AskUserAnswer {
                answers: vec![Answer::single(1).with_custom("alt: skip")],
            },
        };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains(r#""type":"question_answer""#), "got: {j}");
        assert!(j.contains(r#""request_id":"q-1""#), "got: {j}");
        let back: UserInputItem = serde_json::from_str(&j).unwrap();
        if let UserInputItem::QuestionAnswer {
            request_id,
            answers,
        } = back
        {
            assert_eq!(request_id, "q-1");
            assert_eq!(answers.answers.len(), 1);
            assert_eq!(answers.answers[0].selected, vec![1]);
            assert_eq!(answers.answers[0].custom.as_deref(), Some("alt: skip"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn approval_policy_default_is_auto() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Auto);
    }

    #[test]
    fn permission_mode_default_is_auto() {
        assert_eq!(PermissionMode::default(), PermissionMode::Auto);
    }

    #[test]
    fn permission_mode_serde_uses_snake_case() {
        let s = serde_json::to_string(&PermissionMode::Prompt).unwrap();
        assert_eq!(s, "\"prompt\"");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Prompt);
    }

    #[test]
    fn permission_mode_plan_serde_roundtrip() {
        // v1.x: Plan 变体必须能被 serde 序列化与反序列化,且 wire 格式为 `"plan"`。
        let s = serde_json::to_string(&PermissionMode::Plan).unwrap();
        assert_eq!(s, "\"plan\"", "Plan 应序列化为小写字符串");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Plan);
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
    }

    #[test]
    fn permission_mode_accept_edits_and_bubble_serde_roundtrip() {
        for mode in [PermissionMode::AcceptEdits, PermissionMode::Bubble] {
            let s = serde_json::to_string(&mode).unwrap();
            let back: PermissionMode = serde_json::from_str(&s).unwrap();
            assert_eq!(back, mode);
        }
        assert_eq!(PermissionMode::AcceptEdits.as_str(), "accept_edits");
        assert_eq!(PermissionMode::Bubble.as_str(), "bubble");
    }

    #[test]
    fn permission_mode_bypass_serde_roundtrip() {
        // Bypass 必须能被 serde 序列化与反序列化,且 wire 格式为 `"bypass"`。
        let s = serde_json::to_string(&PermissionMode::Bypass).unwrap();
        assert_eq!(s, "\"bypass\"", "Bypass 应序列化为小写字符串");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Bypass);
        assert_eq!(PermissionMode::Bypass.as_str(), "bypass");
    }

    #[test]
    fn permission_mode_ui_cycle_is_three_step() {
        // v1.4: Auto → AcceptEdits → Plan → Auto (Bypass 已移除)
        assert_eq!(
            PermissionMode::Auto.next_in_ui_cycle(),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            PermissionMode::AcceptEdits.next_in_ui_cycle(),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::Plan.next_in_ui_cycle(),
            PermissionMode::Auto
        );
        // Bypass 不在循环内，从 Bypass 会回到 Auto
        assert_eq!(
            PermissionMode::Bypass.next_in_ui_cycle(),
            PermissionMode::Auto
        );
    }

    #[test]
    fn permission_mode_ui_cycle_non_default_falls_to_auto() {
        // Prompt / Deny / Bubble 不进默认循环,从它们循环会落到 Auto。
        for m in [
            PermissionMode::Prompt,
            PermissionMode::Deny,
            PermissionMode::Bubble,
        ] {
            assert_eq!(m.next_in_ui_cycle(), PermissionMode::Auto, "from {m:?}");
        }
    }

    #[test]
    fn bypass_does_not_auto_approve() {
        // v1.4: Bypass 不再静默放行所有工具，仅保留反序列化兼容。
        for tool in ["bash", "write", "Edit", "delete", "read", "custom"] {
            assert!(
                !PermissionMode::Bypass.auto_approves_tool(tool),
                "Bypass should NOT auto-approve {tool}"
            );
        }
    }

    #[test]
    fn session_configured_event_missing_context_window_defaults_none() {
        // 旧 wire payload(无 context_window_size 字段)反序列化时该字段应为 None。
        let j = r#"{
            "session_id": "00000000-0000-0000-0000-000000000000",
            "model": "claude-sonnet-4-latest",
            "provider": "anthropic",
            "approval_policy": "auto",
            "sandbox_policy": "workspace_only"
        }"#;
        let sc: SessionConfiguredEvent = serde_json::from_str(j).unwrap();
        assert_eq!(sc.context_window_size, None);
    }

    #[test]
    fn is_edit_tool_name_matches_write_edit_delete() {
        assert!(is_edit_tool_name("write"));
        assert!(is_edit_tool_name("Edit"));
        assert!(!is_edit_tool_name("bash"));
    }

    #[test]
    fn plan_id_is_unique_and_roundtrips() {
        // PlanId 必须是会话级唯一;透明 newtype 直接序列化底层 UUID。
        let a = PlanId::new();
        let b = PlanId::new();
        assert_ne!(a, b);
        let j = serde_json::to_string(&a).unwrap();
        // `#[serde(transparent)]` 直接输出 UUID 字符串,无额外包装。
        let back: PlanId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }
}
