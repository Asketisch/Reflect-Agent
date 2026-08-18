//! 工具配对保留的辅助函数。
//!
//! microcompact 与 smart_prune 都会丢弃或截断单条消息,但 `Assistant`
//! 消息中的 `tool_use` 块与其对应的 `ChatMessage::Tool`(通过 `call_id`
//! 匹配)必须始终一同保留 —— OpenAI / Anthropic 会拒绝半截配对。
//! 本模块提供 `expand_keep_window_to_preserve_pairs`,让 strategy 层
//! 能够预算一个扩展后的 `keep_start`,从而保护每一对配对。

use std::collections::HashMap;

use reflect_llm::ChatMessage;

/// 为每条 `Assistant` 消息中发出的 `tool_use` id 建立索引,映射到消息
/// 在列表中的位置。
pub fn find_tool_use_indices(msgs: &[ChatMessage]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        if let ChatMessage::Assistant(a) = m {
            for tc in &a.tool_calls {
                out.insert(tc.id.clone(), i);
            }
        }
    }
    out
}

/// 为每条 `ChatMessage::Tool` 的 `call_id` 建立索引,映射到其位置。
pub fn find_tool_result_indices(msgs: &[ChatMessage]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        if let ChatMessage::Tool(t) = m {
            out.insert(t.call_id.clone(), i);
        }
    }
    out
}

/// 扫描每条 `Assistant` 消息的 `tool_calls`,构建 `call_id → tool_name`
/// 映射。
///
/// 这使得 microcompact / smart_prune 在不依赖 `call_id` 中
/// `tool:<name>:` 前缀(生产路径从不应用该前缀)的情况下,也能知道
/// 某个 `Tool` 结果是由*哪个*工具产生的。
pub fn build_call_id_to_tool_name_map(msgs: &[ChatMessage]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for m in msgs {
        if let ChatMessage::Assistant(a) = m {
            for tc in &a.tool_calls {
                out.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }
    out
}

/// 给定候选 `keep_start`(即 `keep_start..n` 区间的消息被原样保留),
/// 将其扩展,以同时纳入「其 `tool_use` 伙伴在保留窗口内、但自身不在」
/// (反之亦然)的消息。
///
/// 返回新的(可能更小的)`keep_start`。
pub fn expand_keep_window_to_preserve_pairs(msgs: &[ChatMessage], keep_start: usize) -> usize {
    let tool_use_idx = find_tool_use_indices(msgs);
    let tool_result_idx = find_tool_result_indices(msgs);
    let mut new_keep_start = keep_start;

    // 遍历保留窗口。对每个 Tool 结果,确保其 Assistant 也位于窗口内
    // (否则扩展);对每个含 tool_use 的 Assistant,确保其 Tool 也位于
    // 窗口内。
    for (_i, m) in msgs.iter().enumerate().skip(keep_start) {
        match m {
            ChatMessage::Tool(t) => {
                if let Some(&use_idx) = tool_use_idx.get(&t.call_id)
                    && use_idx < new_keep_start
                {
                    new_keep_start = use_idx;
                }
            }
            ChatMessage::Assistant(a) => {
                for tc in &a.tool_calls {
                    if let Some(&res_idx) = tool_result_idx.get(&tc.id)
                        && res_idx < new_keep_start
                    {
                        new_keep_start = res_idx;
                    }
                }
            }
            _ => {}
        }
    }
    new_keep_start
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::{AssistantContent, ContentBlock, ToolCallRequest, ToolResult, UserContent};

    fn assistant_with_tools(ids: &[&str]) -> ChatMessage {
        ChatMessage::Assistant(AssistantContent {
            text: None,
            tool_calls: ids
                .iter()
                .map(|id| ToolCallRequest {
                    id: id.to_string(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                })
                .collect(),
            thinking: None,
        })
    }

    fn tool_result(call_id: &str) -> ChatMessage {
        ChatMessage::Tool(ToolResult {
            call_id: call_id.into(),
            content: vec![ContentBlock::text("out")],
            is_error: false,
        })
    }

    #[test]
    fn pair_inside_window_no_expansion() {
        let msgs = vec![
            ChatMessage::User(UserContent { blocks: vec![] }),
            assistant_with_tools(&["c1"]),
            tool_result("c1"),
            ChatMessage::User(UserContent { blocks: vec![] }),
            assistant_with_tools(&["c2"]),
            tool_result("c2"),
        ];
        // keep_start = 3 → 窗口含下标 3..6,已覆盖
        // c2 的 Assistant + Tool。无需扩展。
        let expanded = expand_keep_window_to_preserve_pairs(&msgs, 3);
        assert_eq!(expanded, 3);
    }

    #[test]
    fn pair_straddles_window_expand_backwards() {
        // 下标:0=User, 1=A(c1), 2=T(c1), 3=A(c2), 4=T(c2), 5=User
        // keep_start=4 → 窗口是 [A(c2), T(c2)],但 A(c2) 在 3,所以
        // 需要向前扩展到 3(A(c2) 的 T(c2) 伙伴检查会把下标 4
        // 拉进来,但它已在窗口内)。
        let msgs = vec![
            ChatMessage::User(UserContent { blocks: vec![] }),
            assistant_with_tools(&["c1"]),
            tool_result("c1"),
            assistant_with_tools(&["c2"]),
            tool_result("c2"),
            ChatMessage::User(UserContent { blocks: vec![] }),
        ];
        let expanded = expand_keep_window_to_preserve_pairs(&msgs, 4);
        assert!(
            expanded <= 3,
            "should expand to include A(c2), got {expanded}"
        );
    }

    #[test]
    fn tool_result_in_window_assistant_outside_expands() {
        // 下标 2 = T(c1),但下标 1 的 Assistant(c1) 在保留窗口外
        // → 必须向前扩展到 1。
        let msgs = vec![
            assistant_with_tools(&["c1"]),                     // 0
            ChatMessage::User(UserContent { blocks: vec![] }), // 1
            tool_result("c1"),                                 // 2
        ];
        let expanded = expand_keep_window_to_preserve_pairs(&msgs, 2);
        assert_eq!(expanded, 0, "must pull Assistant(c1) back into window");
    }

    #[test]
    fn tool_result_outside_window_assistant_in_window_expands() {
        // 下标 1 = A(c1);下标 2 的 T(c1) 在窗口内,但检查是双向的:
        // A(c1) 的工具调用 id "c1" 找到的 T 在 2、位于窗口内,
        // 无需扩展。
        // 边界情形:下标 1 的 A(c1) 与下标 0 的 T(c1) —— T 在外,A 在内。
        let msgs = vec![
            tool_result("c1"),                                 // 0
            assistant_with_tools(&["c1"]),                     // 1
            ChatMessage::User(UserContent { blocks: vec![] }), // 2
        ];
        // keep_start=1 → 窗口是 [A(c1), User]。A(c1) 需要位于
        // 下标 0 的 T(c1) → 向前扩展到 0。
        let expanded = expand_keep_window_to_preserve_pairs(&msgs, 1);
        assert_eq!(expanded, 0);
    }
}
