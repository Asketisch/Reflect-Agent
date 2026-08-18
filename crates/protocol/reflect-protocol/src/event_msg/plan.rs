//! v1.x Plan mode 生命周期 + plan step 载荷。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::item::{PermissionMode, PlanId};

/// v1.x Plan mode: agent 请求进入 Plan mode。等用户审批后 core 才
/// 把 `PermissionMode` 切到 `Plan`。
///
/// `task` 是用户/agent 描述的规划目标(如 `"refactor auth module"`),
/// TUI 弹窗和 approval reason 都会展示。`plan_id` 让 TUI 能把用户的
/// 审批决策(`Op::PlanApproval { id, choice }`)准确回送给 core 在
/// `plan_approval_gate` 里等待的对应 waiter —— 没有它,enter 路径会
/// 永久阻塞在 oneshot 上(用户无处确认)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRequestEvent {
    /// 与 `Op::PlanApproval.id` 配对;core 在 `dispatch_plan_request` 里
    /// 生成并注册到 gate 后,必须把同一个 id 透传给 TUI,否则 waiter 永不唤醒。
    #[serde(default)]
    pub plan_id: PlanId,
    pub task: String,
}

/// v1.x Plan mode: agent 调研结束,plan markdown 已生成。等用户审批
/// 后 core 把 `PermissionMode` 切回 `Prompt`,写工具解锁。
///
/// `markdown` 是 plan 的完整内容,通常由 agent 把最近的 tool 调研
/// 结果汇总成 markdown。`plan_id` 用于前后端配对 `PlanApproved` /
/// `PlanRejected`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanReadyEvent {
    pub plan_id: PlanId,
    pub markdown: String,
    /// plan markdown 落盘路径(`<workspace>/.reflect/plan/<plan_id>.md`)。
    ///
    /// core 在 `dispatch_plan_ready` 里把 plan 写成文件,使其成为可引用、
    /// 可 `cat` 的持久产物(而非只在内存里飘一次的事件载荷)。写盘为
    /// best-effort,失败时为 `None`(降级为旧的无文件行为)。
    ///
    /// `#[serde(default)]` 保证旧客户端/旧快照向前兼容(无 `path` 字段时反序列化为 `None`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// v1.x Plan mode: 用户在 approval modal 上 approve plan。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanApprovedEvent {
    pub plan_id: PlanId,
}
/// v1.x Plan mode: 用户在 approval modal 上 reject plan。`reason` 是
/// 可选的用户反馈文本(v1.x 暂未在 TUI 收集,留 `None`;后续 v1.x+1
/// 加 review comment 时填具体原因)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRejectedEvent {
    pub plan_id: PlanId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// v1.x Plan mode 草稿预览:agent 在 Plan mode 下用 write/PlanWrite 把
/// plan markdown 落到 `<workspace>/.reflect/plan/<name>.md` 时,core 在
/// tool_exec 末尾 emit 此事件,让 TUI 即时把 plan 草稿推到对话流。
///
/// **与 `PlanReadyEvent` 的关键区别**:
/// - **不阻塞** —— 不注册 `PlanApprovalGate` waiter,不打开 approval modal,
///   agent turn 不暂停;用户看到的是「正在演化」的草稿,可继续迭代。
/// - **触发时机** —— 每次 plan 文件写盘成功就发(可能多次,覆盖式更新)。
///   `PlanReady` 只发一次,在 agent 调 `ExitPlanMode` 后。
/// - **决策路径** —— 用户做 1/2/3 选择仍走 `PlanReady` → `plan_approval`
///   弹窗;`PlanDraftUpdated` 只刷新可见性。
///
/// `draft_id` 是落盘文件名(不含路径,如 `refactor.md`),用于 TUI 在多
/// 次草稿更新时辨认来源;`path` 是绝对路径,便于跳转/`cat`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanDraftUpdatedEvent {
    /// 草稿源文件名(不含目录),用于 TUI 标识「这是哪个草稿的更新」。
    pub draft_id: String,
    /// plan markdown 全文(从文件读出),TUI 直接渲染。
    pub markdown: String,
    /// 草稿落盘绝对路径(`<workspace>/.reflect/plan/<draft_id>`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// v1.x Plan mode: `PermissionMode` 状态机切换通知。
///
/// `from` / `to` 都填便于客户端追溯;通常 `to` 是 `Plan`(进入)
/// 或 `Prompt`(退出批准后回到 Prompt)。任何订阅方(hook 引擎、
/// tool queue、TUI status bar)都根据这个事件更新本地视图。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionModeChangedEvent {
    pub from: PermissionMode,
    pub to: PermissionMode,
}

// ── Plan step events (批次二十四 #14) ───────────────────────────────────────

/// 一个 plan step 的生命周期状态。`Pending`(尚未开始)、`InProgress`(agent 正在执行)、`Done`(完成)、
/// `Skipped`(agent 判定不适用)。TUI checkbox:`[ ]` / `[~]` / `[x]` / `[-]`。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    /// 尚未开始(默认)。
    Pending,
    /// 正在执行(agent emit `PlanStep` 时若先标 InProgress,可显示 spinner)。
    InProgress,
    /// 已完成。
    Done,
    /// 被跳过(agent 判定该 step 不适用)。
    Skipped,
}

/// 批次二十四(#14):单个 plan step 的状态变更。`plan_id` 与 `PlanReady`
/// 配对;`index` 是该 step 在 plan 中的序号(0 起);`total` 是已知 step
/// 总数(允许 agent 边执行边追加,故 total 可变);`title` 是 step 的简短
/// 描述(从 plan markdown 提取的清单项文本)。TUI reducer 维护
/// `Vec<PlanStepState>`,按 `index` 就地更新或追加。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanStepEvent {
    pub plan_id: PlanId,
    pub index: usize,
    /// 已知 step 总数(0 = 未知 / 动态)。TUI 取 max(已知 total, index+1)。
    #[serde(default)]
    pub total: usize,
    pub status: PlanStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_ready_event_omits_path_when_none() {
        // 序列化时 path=None 不出现在 JSON 里(skip_serializing_if)。
        let ev = PlanReadyEvent {
            plan_id: PlanId::new(),
            markdown: "## Plan".into(),
            path: None,
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(!j.contains(r#""path""#), "path should be omitted, got: {j}");
    }

    #[test]
    fn plan_ready_event_serializes_path_when_some() {
        let ev = PlanReadyEvent {
            plan_id: PlanId::new(),
            markdown: "## Plan".into(),
            path: Some(PathBuf::from("/ws/.reflect/plan/abc.md")),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(
            j.contains(r#""path":"/ws/.reflect/plan/abc.md""#),
            "path should be serialized, got: {j}"
        );
    }

    #[test]
    fn plan_ready_event_deserializes_legacy_json_without_path() {
        // 旧客户端/旧快照的 JSON 没有 path 字段,必须能反序列化为 path=None。
        // 这是向前兼容的关键不变量。
        // 用转义字符串而非 raw literal,避免 JSON 末尾 `"}` 与 `r##"..."##`
        // 终止符冲突(Edition 2024 reserved multi-hash token)。
        let legacy =
            "{\"plan_id\":\"00000000-0000-0000-0000-000000000000\",\"markdown\":\"## Plan\"}";
        let ev: PlanReadyEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(ev.markdown, "## Plan");
        assert!(ev.path.is_none(), "legacy JSON should yield path=None");
    }
}
