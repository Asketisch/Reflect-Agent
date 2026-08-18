//! `HookEvent` —— 引擎派发的 5 类事件。
//!
//! 参见 `docs/tools-and-hooks.md §4.1`。注意:`HookEvent` **不**直接
//! 携带 `ToolContext`(后者位于 `reflect-tools`),以避免依赖循环。
//! 需要上下文的 hook 可读取此处暴露的精简字段
//! (workspace、session_id、turn_id、permission_mode)。
//! 完整的 `ToolContext` 可由工具实现通过 `Tool::execute` 获得。

use serde::{Deserialize, Serialize};

use reflect_protocol::{PermissionMode, ThreadId, ToolError, ToolOutput, TurnId};

/// `HookEvent` 的种类标签 —— 供 `Hook::events` 过滤使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    SessionStart,
    // ── v1.1.0:task 生命周期事件(reflect-task) ──
    /// `TaskCreate` 工具成功落盘后触发。
    TaskCreated,
    /// `TaskUpdate` 把 status 改为 `completed` 时触发(已存在任务)。
    TaskCompleted,
    /// `TaskUpdate` 改了除 status→completed 之外的字段时触发。
    TaskUpdated,
}

/// 触发 stop 的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型发出最终消息(无工具调用)。
    AgentDecision,
    /// 图执行达到 `max_iterations` 安全阀。
    MaxIterations,
    /// 用户中断(Ctrl-C)。
    UserInterrupt,
}

/// 暴露给 hook 的 `ToolContext` 精简视图(避免 tools↔hooks 循环)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    pub session_id: ThreadId,
    pub turn_id: TurnId,
    pub workspace: std::path::PathBuf,
    pub permission_mode: PermissionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookEvent {
    /// 工具运行前触发。hook 可返回 `Deny`、`ModifyArgs` 或
    /// `PermissionOverride`。
    PreToolUse {
        tool: String,
        args: serde_json::Value,
        ctx: HookContext,
    },
    /// 工具成功返回后触发。
    PostToolUse {
        tool: String,
        result: ToolOutput,
        elapsed_ms: u64,
    },
    /// 工具返回错误(或超时)后触发。
    PostToolUseFailure {
        tool: String,
        error: ToolError,
        elapsed_ms: u64,
    },
    /// agent 希望结束本 turn 时触发。hook 可返回 `Deny` 强制继续。
    Stop { reason: StopReason, attempt: u32 },
    /// 会话启动时触发一次。hook 接收已解析的 config。
    SessionStart {
        session_id: ThreadId,
        config: serde_json::Value,
    },
    // ── v1.1.0:task 生命周期事件(reflect-task) ──
    /// `TaskCreate` 工具成功落盘后触发。`task` 是任务的 JSON 快照
    /// (使用 `serde_json::Value` 避免 `hooks ↔ task` 反向依赖)。
    TaskCreated { task: serde_json::Value },
    /// `TaskUpdate` 把 status 改为 `completed` 时触发。`previous_status`
    /// 是变更前的状态字符串(`"pending"` / `"in_progress"` / `"deleted"`)。
    TaskCompleted {
        task: serde_json::Value,
        previous_status: String,
    },
    /// `TaskUpdate` 改了除 status→completed 之外的字段时触发。
    TaskUpdated {
        task: serde_json::Value,
        changed_fields: Vec<String>,
    },
}

impl HookEvent {
    /// 返回当前事件的种类(供 `Hook::events` 过滤)。
    pub fn kind(&self) -> HookEventKind {
        match self {
            HookEvent::PreToolUse { .. } => HookEventKind::PreToolUse,
            HookEvent::PostToolUse { .. } => HookEventKind::PostToolUse,
            HookEvent::PostToolUseFailure { .. } => HookEventKind::PostToolUseFailure,
            HookEvent::Stop { .. } => HookEventKind::Stop,
            HookEvent::SessionStart { .. } => HookEventKind::SessionStart,
            HookEvent::TaskCreated { .. } => HookEventKind::TaskCreated,
            HookEvent::TaskCompleted { .. } => HookEventKind::TaskCompleted,
            HookEvent::TaskUpdated { .. } => HookEventKind::TaskUpdated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn kind_matches_variant() {
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(e.kind(), HookEventKind::Stop);
    }

    #[test]
    fn context_serde_roundtrip() {
        let c = HookContext {
            session_id: ThreadId::new(),
            turn_id: TurnId::new(),
            workspace: PathBuf::from("/tmp"),
            permission_mode: PermissionMode::Auto,
        };
        let back: HookContext = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.workspace, PathBuf::from("/tmp"));
    }
}
