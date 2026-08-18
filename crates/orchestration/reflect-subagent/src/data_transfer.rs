//! 父级与子代理之间的数据流。
//!
//! - [`DataTransferConfig::pass_context_messages`] —— 从父级复制到子 agent 初始消息列表
//!   的尾部消息数量。
//! - [`ResultExtractor`] —— 如何从 `TurnHandle` 事件流中抽取子 agent 的最终回答,
//!   并返回给父级的工具。

use reflect_llm::ChatMessage;
use reflect_protocol::{ContentBlock, Event, EventMsg};

/// 单次 `spawn()` 调用时父↔子数据流的配置。
#[derive(Debug, Clone)]
pub struct DataTransferConfig {
    /// 从父级 `state.messages` 末尾复制并前置到子 agent 初始 prompt 的消息数。
    /// `0` 表示子 agent 仅接收自己的 system prompt + 工具传入的 user input。
    pub pass_context_messages: usize,
    /// 如何从子 agent 事件流中抽取最终回答。
    pub result_extractor: ResultExtractor,
}

impl Default for DataTransferConfig {
    fn default() -> Self {
        Self {
            pass_context_messages: 0,
            result_extractor: ResultExtractor::LastAssistantText,
        }
    }
}

/// 从子 agent 事件流中挑选回复的策略。
#[derive(Debug, Clone)]
pub enum ResultExtractor {
    /// 取最后一个 `AgentMessage` 的文本(或在 `TurnComplete` 之前拼接
    /// 所有 `AgentMessageDelta`)。
    LastAssistantText,
    /// 取 `call_id` 与给定字符串匹配的 `ToolResult` 的 `content`。
    /// 用于子 agent 应调用某个特定终止工具(例如 `finish_subagent`)的场景。
    LastToolResult(String),
}

/// 遍历 `TurnHandle` 的事件流,按所选 [`ResultExtractor`] 抽取回答。
/// 若流结束但没有匹配事件,返回 `None`。
pub fn extract_result(events: &[Event], extractor: &ResultExtractor) -> Option<String> {
    match extractor {
        ResultExtractor::LastAssistantText => {
            let mut acc = String::new();
            for e in events {
                match &e.msg {
                    EventMsg::AgentMessageDelta(d) => acc.push_str(&d.delta),
                    EventMsg::AgentMessage(m) => return Some(m.text.clone()),
                    _ => {}
                }
            }
            if acc.is_empty() { None } else { Some(acc) }
        }
        ResultExtractor::LastToolResult(want_id) => {
            for e in events.iter().rev() {
                if let EventMsg::ToolCallEnd(end) = &e.msg {
                    if &end.call_id == want_id {
                        for c in &end.output.content {
                            if let ContentBlock::Text { text } = c {
                                return Some(text.clone());
                            }
                        }
                    }
                }
            }
            None
        }
    }
}

/// 子代理返回后附加 `[Coordinator Principle]` footer(父协调者 reminder)。
pub fn append_coordinator_principle_footer(result: &str, footer: Option<&str>) -> String {
    let Some(footer) = footer.filter(|f| !f.trim().is_empty()) else {
        return result.to_string();
    };
    format!("{result}\n\n[Coordinator Principle]\n{footer}")
}

/// 构造子 agent 的初始消息列表:复制父级末尾 `pass_context_messages`
/// 条 chat 消息,然后追加新传入的 user input。
pub fn build_child_initial_messages(
    parent_tail: &[ChatMessage],
    user_input: ChatMessage,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = parent_tail.to_vec();
    out.push(user_input);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{AgentMessage, Event, ToolOutput};

    fn evt_text(t: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::AgentMessage(AgentMessage { text: t.into() }),
        )
    }

    fn evt_delta(d: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::AgentMessageDelta(reflect_protocol::AgentMessageDelta { delta: d.into() }),
        )
    }

    fn evt_tool_result(call_id: &str, text: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::ToolCallEnd(reflect_protocol::ToolCallEndEvent {
                call_id: call_id.into(),
                output: ToolOutput {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                },
                is_error: false,
                elapsed_ms: 0,
                child_id: None,
            }),
        )
    }

    #[test]
    fn extract_last_assistant_text_from_delta_acc() {
        let events = vec![evt_delta("hel"), evt_delta("lo")];
        let got = extract_result(&events, &ResultExtractor::LastAssistantText);
        assert_eq!(got, Some("hello".into()));
    }

    #[test]
    fn extract_last_assistant_text_from_message_event() {
        let events = vec![evt_delta("hel"), evt_text("lo")];
        let got = extract_result(&events, &ResultExtractor::LastAssistantText);
        assert_eq!(got, Some("lo".into()));
    }

    #[test]
    fn extract_last_tool_result() {
        let events = vec![
            evt_tool_result("c1", "first"),
            evt_tool_result("c2", "second"),
        ];
        let got = extract_result(&events, &ResultExtractor::LastToolResult("c2".into()));
        assert_eq!(got, Some("second".into()));
    }

    #[test]
    fn extract_returns_none_when_no_match() {
        let events = vec![evt_tool_result("c1", "x")];
        let got = extract_result(&events, &ResultExtractor::LastToolResult("missing".into()));
        assert!(got.is_none());
    }

    #[test]
    fn build_child_messages_appends_user_input() {
        use reflect_llm::{ContentBlock, UserContent};
        let parent = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("old1")],
        })];
        let new = ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("new")],
        });
        let msgs = build_child_initial_messages(&parent, new);
        assert_eq!(msgs.len(), 2);
    }
}
