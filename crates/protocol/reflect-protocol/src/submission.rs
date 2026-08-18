//! Submission —— client → core 的命令单元。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::op::Op;

/// Submission 表示客户端向 core 发起的一条命令。
///
/// `id` 用于把随之产生的事件(均携带同一 `id`)关联回原始 Submission。
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
}

impl Submission {
    /// 构造一条 UserInput Submission,自动生成 id。
    pub fn user_input(text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            op: crate::op::Op::user_input_text(text),
            client_user_message_id: None,
            trace: None,
        }
    }

    /// 构造任意 Submission 并预置 id。
    pub fn with_id(id: impl Into<String>, op: Op) -> Self {
        Self {
            id: id.into(),
            op,
            client_user_message_id: None,
            trace: None,
        }
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
