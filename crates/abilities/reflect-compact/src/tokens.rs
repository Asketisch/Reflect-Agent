//! Token 估算。本函数为近似估算,**不能**作为计费级计数器。
//!
//! 启发式:`len(text) / 3.5` 字符每 token,image 块按 1000 token 计。
//! 对应 reflect 的 `graph.py:_estimate_tokens`。

use reflect_llm::{ChatMessage, ContentBlock, ToolResult};

/// 估算一段聊天消息的总 token 数。纯函数 —— 不调用任何 LLM。
/// 用于触发压缩策略;不可替代真实分词器(如 tiktoken)。
pub fn estimate_messages(messages: &[ChatMessage]) -> u32 {
    messages.iter().map(estimate_message).sum()
}

fn estimate_message(msg: &ChatMessage) -> u32 {
    match msg {
        ChatMessage::System(s) => chars_to_tokens(s),
        ChatMessage::User(u) => u.blocks.iter().map(estimate_block).sum(),
        ChatMessage::Assistant(a) => {
            let mut t = 0;
            if let Some(text) = &a.text {
                t += chars_to_tokens(text);
            }
            if let Some(thinking) = &a.thinking {
                t += chars_to_tokens(thinking);
            }
            // tool_calls:每个调用的 JSON 参数均计入。
            for tc in &a.tool_calls {
                t += chars_to_tokens(&tc.arguments.to_string());
                t += chars_to_tokens(&tc.name);
            }
            t
        }
        ChatMessage::Tool(r) => chars_to_tokens(&tool_result_text(r)),
    }
}

fn estimate_block(b: &ContentBlock) -> u32 {
    match b {
        ContentBlock::Text { text } => chars_to_tokens(text),
        // 在 reflect 中,图片属于固定大预算块。
        ContentBlock::Image { .. } => 1000,
    }
}

fn tool_result_text(r: &ToolResult) -> String {
    // content 现为 Vec<ContentBlock>(多模态);为 token 估算展平为
    // 文本。Image 块按占位符计算,而非原始字节长度(后者会严重
    // 高估 token 数)。
    r.content_as_text()
}

fn chars_to_tokens(s: &str) -> u32 {
    // 直接向上取整到 u32,以保持算术精确。
    ((s.chars().count() as f64) / 3.5).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::{AssistantContent, ContentBlock, ToolCallRequest, ToolResult, UserContent};

    #[test]
    fn empty_messages_yield_zero() {
        assert_eq!(estimate_messages(&[]), 0);
    }

    #[test]
    fn short_text_rounds_up() {
        // 4 字符 / 3.5 = 1.14,向上取整 = 2
        let msgs = vec![ChatMessage::System("abcd".into())];
        assert_eq!(estimate_messages(&msgs), 2);
    }

    #[test]
    fn long_text_uses_3_5_ratio() {
        // 35 字符 / 3.5 = 10
        let s: String = "a".repeat(35);
        let msgs = vec![ChatMessage::System(s)];
        assert_eq!(estimate_messages(&msgs), 10);
    }

    #[test]
    fn image_block_equals_1000_tokens() {
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Image {
                data: vec![0xff; 100],
                mime_type: "image/png".into(),
            }],
        })];
        assert_eq!(estimate_messages(&msgs), 1000);
    }

    #[test]
    fn assistant_counts_text_thinking_and_tool_calls() {
        let a = AssistantContent {
            text: Some("hello".into()),
            tool_calls: vec![ToolCallRequest {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "/tmp/x"}),
            }],
            thinking: Some("thinking about it".into()),
        };
        let msgs = vec![ChatMessage::Assistant(a)];
        let t = estimate_messages(&msgs);
        // "hello" = ceil(5/3.5)=2;"thinking about it" = ceil(18/3.5)=6;
        // "read" = ceil(4/3.5)=2;args JSON 若干
        assert!(t >= 10);
    }

    #[test]
    fn tool_result_counts_content() {
        let r = ToolResult {
            call_id: "c1".into(),
            content: vec![ContentBlock::text("ok result")],
            is_error: false,
        };
        let msgs = vec![ChatMessage::Tool(r)];
        let t = estimate_messages(&msgs);
        // "ok result" = ceil(9/3.5) = 3
        assert_eq!(t, 3);
    }

    #[test]
    fn sums_across_messages() {
        let msgs = vec![
            ChatMessage::System("hello".into()),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("world")],
            }),
        ];
        let t = estimate_messages(&msgs);
        // "hello"=2,"world"=2
        assert_eq!(t, 4);
    }
}
