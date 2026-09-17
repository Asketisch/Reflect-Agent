//! Submission —— client → core 的命令单元。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::op::Op;

/// Submission 表示客户端向 core 发起的一条命令。
///
/// `id` 用于把随之产生的事件(均携带同一 `id`)关联回原始 Submission。
///
/// v1.x 新增顶层 `workspace` 可选字段 —— Tauri GUI 在创建/恢复会话时把
/// 当前激活工作区注入,后端 `submission_loop` 收到首条 UserInput 时取该
/// 值(或回退 `cfg.current_workspace()`)写入 `RolloutRecord::SessionMeta.workspace`。
/// 不在 `Op::UserInput` 加字段是为了避免破坏既有 `match` 派生。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub op: Op,
    /// 客户端提供的用户消息 ID(用于跨 rollout 日志的链路追踪)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    /// W3C trace 上下文,用于跨进程链路追踪。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<W3cTraceContext>,
    /// v1.x:会话归属工作区(绝对路径字符串)。`None` = 不指定(CLI / 测试场景)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// v1.x:用户文本来自插件 slash 命令展开时的命令全名
    /// (`/plugin:ns:name args` → `plugin:ns:name`)。`None` = 普通 prompt。
    /// 仅作 rollout / 遥测的来源标注,不影响 core 语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_command: Option<String>,
}

impl Submission {
    /// 构造一条 UserInput Submission,自动生成 id。
    pub fn user_input(text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            op: crate::op::Op::user_input_text(text),
            client_user_message_id: None,
            trace: None,
            workspace: None,
            source_command: None,
        }
    }

    /// 构造任意 Submission 并预置 id。
    pub fn with_id(id: impl Into<String>, op: Op) -> Self {
        Self {
            id: id.into(),
            op,
            client_user_message_id: None,
            trace: None,
            workspace: None,
            source_command: None,
        }
    }

    /// v1.x:便捷构造器 —— UserInput + workspace 注入。
    pub fn user_input_in_workspace(text: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            op: crate::op::Op::user_input_text(text),
            client_user_message_id: None,
            trace: None,
            workspace: Some(workspace.into()),
            source_command: None,
        }
    }

    /// v1.x:链式标注来源命令(配合 `reflect_plugin::expand_user_input`
    /// 使用,记录该 prompt 由哪个插件命令展开而来)。
    pub fn with_source_command(mut self, command: impl Into<String>) -> Self {
        self.source_command = Some(command.into());
        self
    }
}

/// W3C trace 上下文(W3C Trace Context 规范的子集)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct W3cTraceContext {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_flags: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;

    #[test]
    fn user_input_helper_creates_unique_ids() {
        let a = Submission::user_input("hi");
        let b = Submission::user_input("hi");
        assert_ne!(a.id, b.id);
        assert!(matches!(a.op, Op::UserInput { .. }));
    }

    #[test]
    fn serde_roundtrip() {
        let s = Submission::user_input("hello");
        let json = serde_json::to_string(&s).unwrap();
        let back: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.op.discriminant(), s.op.discriminant());
    }
}
