//! v1.1.0 P1 #15:`ask_user` 自由文本询问协议类型。
//!
//! 与 `ask_user_question`(结构化多选题)区分:`ask_user` 只带一条 prompt,
//! 用户在 TUI 单行 modal 输入自由文本,通过 `Op::AskUserInputResponse`
//! 回执给 `ApprovalGate::ask_user`。

use serde::{Deserialize, Serialize};

/// LLM 通过 `ask_user` 工具向用户发起的自由文本询问。
///
/// `secret` / `placeholder` 为 v1.2 P0 新增的向后兼容可选字段:
/// - `secret = Some(true)` 时 TUI 把输入渲染为 `•` 掩码(密码 / API key
///   等敏感输入)。回执 `Op::AskUserInputResponse.text` 仍为明文 —— masked
///   只影响 UI 显示。
/// - `placeholder` 提供空输入时的占位提示。
///
/// 字段均带 `#[serde(default)]`,旧 producer 不发送时反序列化为 `None`
/// (非破坏,参照 `TokenCountEvent.cost_usd` 先例)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserInputEvent {
    pub request_id: String,
    pub prompt: String,
    /// 是否以掩码渲染输入(敏感数据)。默认 `None`(明文)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<bool>,
    /// 空输入时显示的占位提示。默认 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

impl AskUserInputEvent {
    pub fn new(request_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            prompt: prompt.into(),
            secret: None,
            placeholder: None,
        }
    }

    /// Builder:标记为敏感输入(TUI 掩码渲染)。
    pub fn with_secret(mut self, secret: bool) -> Self {
        self.secret = Some(secret);
        self
    }

    /// Builder:设置空输入占位提示。
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_user_input_event_serde_roundtrip() {
        let ev = AskUserInputEvent::new("u1", "Which API key should I use?");
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""request_id":"u1""#), "got: {j}");
        assert!(j.contains("Which API key"), "got: {j}");
        let back: AskUserInputEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.request_id, "u1");
        assert_eq!(back.prompt, "Which API key should I use?");
    }

    #[test]
    fn secret_and_placeholder_roundtrip() {
        let ev = AskUserInputEvent::new("u2", "Enter token")
            .with_secret(true)
            .with_placeholder("paste here");
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""secret":true"#), "got: {j}");
        assert!(j.contains(r#""placeholder":"paste here""#), "got: {j}");
        let back: AskUserInputEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.secret, Some(true));
        assert_eq!(back.placeholder.as_deref(), Some("paste here"));
    }

    #[test]
    fn omitted_secret_and_placeholder_deserialize_as_none() {
        // Wire compatibility: 旧 producer 不发送 secret / placeholder 时,
        // 反序列化为 None(非破坏)。模拟 EventMsg wire 形态的最小 payload。
        let j = r#"{"request_id":"u3","prompt":"p"}"#;
        let back: AskUserInputEvent = serde_json::from_str(j).unwrap();
        assert_eq!(back.secret, None);
        assert_eq!(back.placeholder, None);
    }

    #[test]
    fn secret_true_roundtrips_through_eventmsg_wire() {
        // 确认经 EventMsg wire tag "ask_user_input" 也正确透传。
        use crate::event_msg::EventMsg;
        let payload = r#"{"type":"ask_user_input","request_id":"u4","prompt":"pw","secret":true,"placeholder":"hint"}"#;
        let back: EventMsg = serde_json::from_str(payload).unwrap();
        match back {
            EventMsg::AskUserInput(e) => {
                assert_eq!(e.secret, Some(true));
                assert_eq!(e.placeholder.as_deref(), Some("hint"));
            }
            _ => panic!("wrong variant"),
        }
    }
}
