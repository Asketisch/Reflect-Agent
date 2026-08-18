//! Microcompact:本地启发式压缩,不调用 LLM。
//!
//! 固定保留 `System` 与第一条 `User`;对每条「旧」消息(不在 keep_recent
//! 窗口、不在固定集、且不属于保留工具),将其内容替换为占位符。
//! 剥离 thinking 块(Anthropic 服务端已保留)。返回
//! `(messages, was_compacted)`。
//!
//! 对应 reflect `graph.py:microcompact_messages`(第 330-439 行)。

use reflect_llm::{AssistantContent, ChatMessage, ContentBlock, ToolResult};

/// 其结果**绝不**被替换的工具(其内容对下一次模型调用至关重要)。
/// 对应 reflect 的 `_PRESERVE_TOOL_NAMES`,再加上 v1.1.0 task 系统:
/// `TaskUpdate` 返回的 `updatedFields` / `statusChange` 是后续 turn
/// 决策的关键信号,`TaskCreate` 返回的 task id 是后续
/// TaskGet/TaskUpdate 的入口 —— 二者都不能被 microcompact 抹掉。
pub const PRESERVE_TOOL_NAMES: &[&str] = &[
    "write",
    "replace",
    "edit",
    "todo",
    "TaskCreate",
    "TaskUpdate",
];

/// 其结果**会被**占位符替换的工具。
pub const TRUNCATABLE_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "glob",
    "bash",
    "load_skill",
    "web_fetch",
    "web_search",
];

/// `trigger_tokens` 的默认比例系数,低于此比例时 microcompact 是 no-op。
/// 对应 reflect 的 `microcompact_trigger_ratio = 0.7`。
pub const MICROCOMPACT_TRIGGER_RATIO: f32 = 0.7;

/// microcompact 的默认 `keep_recent` 窗口大小。M5 v0 采用紧致的
/// per-turn 默认值 `4`;此前的 M4 默认值为 30。
pub const KEEP_RECENT_DEFAULT: usize = 4;

/// 替换可截断工具结果时插入的占位符。
pub const TRUNCATE_PLACEHOLDER: &str = "[工具结果已清除]";

/// microcompact 调用结果报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    /// 压缩前的 token 估算。
    pub before_tokens: u32,
    /// 压缩后的 token 估算。
    pub after_tokens: u32,
    /// 内容被替换的消息数量。
    pub removed_count: usize,
    /// 策略是否实际修改了消息列表。
    pub was_compacted: bool,
}

/// microcompact 的可调参数。
#[derive(Debug, Clone)]
pub struct MicrocompactConfig {
    /// 触发压缩的 token 阈值。
    pub trigger_tokens: u32,
    /// 完整保留的最近消息数。
    pub keep_recent: usize,
    /// 当总 token < trigger * ratio 时跳过压缩。
    pub trigger_ratio: f32,
}

impl Default for MicrocompactConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: crate::strategy::DEFAULT_TRIGGER_TOKENS,
            keep_recent: KEEP_RECENT_DEFAULT,
            trigger_ratio: MICROCOMPACT_TRIGGER_RATIO,
        }
    }
}

/// 执行 microcompact。返回新的消息列表与报告。
///
/// 若 `crate::estimate_messages(&messages) * ratio < trigger_tokens`,
/// 输入原样返回(`was_compacted = false`)。
pub fn microcompact(
    messages: Vec<ChatMessage>,
    cfg: &MicrocompactConfig,
) -> (Vec<ChatMessage>, CompactReport) {
    let before = crate::tokens::estimate_messages(&messages);
    let threshold = ((cfg.trigger_tokens as f32) * cfg.trigger_ratio) as u32;
    if before < threshold {
        return (
            messages,
            CompactReport {
                before_tokens: before,
                after_tokens: before,
                removed_count: 0,
                was_compacted: false,
            },
        );
    }

    let n = messages.len();
    if n <= cfg.keep_recent {
        return (
            messages,
            CompactReport {
                before_tokens: before,
                after_tokens: before,
                removed_count: 0,
                was_compacted: false,
            },
        );
    }

    // 收集固定保留下标:每个 System 消息 + 第一条 User 消息。
    let first_user = messages
        .iter()
        .position(|m| matches!(m, ChatMessage::User(_)))
        .unwrap_or(usize::MAX);
    let mut pinned: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, m) in messages.iter().enumerate() {
        if matches!(m, ChatMessage::System(_)) || i == first_user {
            pinned.insert(i);
        }
    }
    // keep_recent 尾部窗口内的消息同样豁免。
    let keep_start = n.saturating_sub(cfg.keep_recent);
    for i in keep_start..n {
        pinned.insert(i);
    }
    // M5:扩展保留窗口以保住每对 tool_use ↔ tool_result,避免向
    // provider 输出半截配对。
    let expanded = crate::tool_pair::expand_keep_window_to_preserve_pairs(&messages, keep_start);
    for i in expanded..keep_start {
        pinned.insert(i);
    }

    let mut removed_count = 0;
    let mut new_messages = Vec::with_capacity(n);
    let call_id_map = crate::tool_pair::build_call_id_to_tool_name_map(&messages);
    for (i, m) in messages.into_iter().enumerate() {
        if pinned.contains(&i) {
            new_messages.push(m);
            continue;
        }
        let (new_m, was_replaced) = replace_compactable(m, &call_id_map);
        if was_replaced {
            removed_count += 1;
        }
        new_messages.push(new_m);
    }

    let after = crate::tokens::estimate_messages(&new_messages);
    (
        new_messages,
        CompactReport {
            before_tokens: before,
            after_tokens: after,
            removed_count,
            was_compacted: removed_count > 0,
        },
    )
}

/// 若消息是 `Tool` 且工具名属于可截断集合,则替换其内容。
/// 返回(可能被替换的)消息及一个 `bool`,标识正文是否实际发生变化。
fn replace_compactable(
    msg: ChatMessage,
    call_id_map: &std::collections::HashMap<String, String>,
) -> (ChatMessage, bool) {
    match msg {
        ChatMessage::Tool(t) => {
            // 通过 Assistant tool_calls 查找生成该结果的工具名,
            // 然后判断是否在 PRESERVE_TOOL_NAMES 中。
            let tool_name = call_id_map
                .get(&t.call_id)
                .map(String::as_str)
                .unwrap_or("");
            if PRESERVE_TOOL_NAMES.contains(&tool_name) {
                (ChatMessage::Tool(t), false)
            } else {
                (
                    ChatMessage::Tool(ToolResult {
                        call_id: t.call_id,
                        content: vec![ContentBlock::Text {
                            text: TRUNCATE_PLACEHOLDER.into(),
                        }],
                        is_error: t.is_error,
                    }),
                    true,
                )
            }
        }
        ChatMessage::Assistant(a) => {
            // 剥离 thinking 块,保留 text + tool_calls。
            let stripped = AssistantContent {
                text: a.text,
                tool_calls: a.tool_calls,
                thinking: None,
            };
            (ChatMessage::Assistant(stripped), a.thinking.is_some())
        }
        other => (other, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::ToolCallRequest;
    use reflect_llm::{AssistantContent, ContentBlock, ToolResult, UserContent};

    fn make_tool_result(call_id: &str, content: &str) -> ChatMessage {
        ChatMessage::Tool(ToolResult {
            call_id: call_id.into(),
            content: vec![ContentBlock::text(content)],
            is_error: false,
        })
    }

    fn make_assistant_with_tool_call(call_id: &str, tool_name: &str) -> ChatMessage {
        ChatMessage::Assistant(AssistantContent {
            text: Some("ok".into()),
            tool_calls: vec![ToolCallRequest {
                id: call_id.into(),
                name: tool_name.into(),
                arguments: serde_json::Value::Null,
            }],
            thinking: None,
        })
    }

    fn make_assistant_with_thinking() -> ChatMessage {
        ChatMessage::Assistant(AssistantContent {
            text: Some("ok".into()),
            tool_calls: vec![],
            thinking: Some("internal".into()),
        })
    }

    #[test]
    fn below_threshold_noop() {
        let msgs = vec![ChatMessage::System("hi".into())];
        let (_, r) = microcompact(msgs.clone(), &MicrocompactConfig::default());
        assert!(!r.was_compacted);
        assert_eq!(r.before_tokens, r.after_tokens);
    }

    #[test]
    fn pinned_system_kept_verbatim() {
        // 布局:
        //   下标 0: System                → 固定保留(System)
        //   下标 1: Assistant (tool_call write)→ 构建 call_id→name 映射
        //   下标 2: Tool c1_write (write)     → 经映射保留
        //   下标 3: Tool c2 (truncatable)     → 位于 keep_recent 尾部(最后 1 条)
        //
        // 压缩后:System 不变,c1 保留,c2 在 keep_recent 尾部
        // (所以不变)。不发生任何替换。
        let msgs = vec![
            ChatMessage::System("core system prompt".into()),
            make_assistant_with_tool_call("c1_write", "write"),
            make_tool_result("c1_write", "important file content"),
            make_tool_result("c2", "bash output here"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 1,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        // 无替换 → 未压缩。
        assert!(!r.was_compacted);
        assert_eq!(r.removed_count, 0);
        match &out[0] {
            ChatMessage::System(s) => assert_eq!(s, "core system prompt"),
            _ => panic!("expected System at 0"),
        }
        match &out[2] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "important file content"),
            _ => panic!("expected Tool at 2"),
        }
        match &out[3] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "bash output here"),
            _ => panic!("expected Tool at 3"),
        }
    }

    #[test]
    fn truncatable_tool_replaced_when_not_pinned() {
        // 6 条消息,keep_recent=2 → 尾部(下标 4,5)豁免。
        // 下标 2(write,经映射保留)不被替换。下标 3(c2,
        // truncatable,不在 keep_recent)**会**被替换。
        let msgs = vec![
            ChatMessage::System("sys".into()),
            make_assistant_with_tool_call("c1_write", "write"),
            make_tool_result("c1_write", "important"),
            make_tool_result("c2", "long bash output data here"),
            make_tool_result("c3", "more data"),
            make_tool_result("c4", "tail data"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 2,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        assert!(r.was_compacted);
        assert_eq!(r.removed_count, 1);
        // 下标 0 是 System。
        assert!(matches!(out[0], ChatMessage::System(_)));
        // 下标 2 是受保留的 write 结果。
        match &out[2] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "important"),
            _ => panic!("expected Tool at 2"),
        }
        // 下标 3 的 c2 → 替换为占位符。
        match &out[3] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), TRUNCATE_PLACEHOLDER),
            _ => panic!("expected Tool at 3"),
        }
        // c3、c4 在 keep_recent 尾部 → 不变。
        match &out[4] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "more data"),
            _ => panic!("expected Tool at 4"),
        }
        match &out[5] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "tail data"),
            _ => panic!("expected Tool at 5"),
        }
    }

    #[test]
    fn truncatable_tool_replaced_keep_recent_1() {
        // 5 条消息,keep_recent=1 → 只有下标 4 在 keep_recent 内。
        // 下标 1(tool)是 truncatable → 被替换。
        let msgs = vec![
            ChatMessage::System("sys".into()),
            make_tool_result("c1", "data here"),
            make_tool_result("c2", "more data"),
            make_tool_result("c3", "even more"),
            make_tool_result("c4", "tail"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 1,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        assert!(r.was_compacted);
        // 下标 1 的 c1 应被截断。
        match &out[1] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), TRUNCATE_PLACEHOLDER),
            _ => panic!("expected Tool at 1"),
        }
        // 下标 4 的 c4 在 keep_recent 尾部 → 不变。
        match &out[4] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "tail"),
            _ => panic!("expected Tool at 4"),
        }
    }

    #[test]
    fn first_user_pinned() {
        let msgs = vec![
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("first user message")],
            }),
            make_tool_result("c1", "data"),
            make_tool_result("c2", "more"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 1,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        assert!(r.was_compacted);
        // 下标 0(首条 User)固定保留。
        match &out[0] {
            ChatMessage::User(_) => {}
            _ => panic!("expected User at 0"),
        }
        // 下标 1 在 keep_recent 尾部(n-1..n)—— 同样保留。
        match &out[2] {
            ChatMessage::Tool(t) => assert_eq!(t.content_as_text(), "more"),
            _ => panic!("expected Tool at 2"),
        }
    }

    #[test]
    fn keep_recent_window_protects_tail() {
        let mut msgs = vec![ChatMessage::System("sys".into())];
        for i in 0..20 {
            msgs.push(make_tool_result(&format!("c{i}"), &format!("data {i}")));
        }
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 5,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        assert!(r.was_compacted);
        // 最后 5 条工具结果应保持不变。
        for (i, slot) in out.iter().enumerate().take(21).skip(16) {
            match slot {
                ChatMessage::Tool(t) => assert!(t.content_as_text().starts_with("data ")),
                _ => panic!("expected Tool at {i}"),
            }
        }
    }

    #[test]
    fn thinking_blocks_stripped() {
        let msgs = vec![
            ChatMessage::System("sys".into()),
            make_assistant_with_thinking(),
            make_tool_result("c1", "x"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 1,
            ..Default::default()
        };
        let (out, r) = microcompact(msgs, &cfg);
        assert!(r.was_compacted);
        match &out[1] {
            ChatMessage::Assistant(a) => {
                assert!(a.thinking.is_none(), "thinking should be stripped");
                assert_eq!(a.text.as_deref(), Some("ok"));
            }
            _ => panic!("expected Assistant at 1"),
        }
    }

    #[test]
    fn no_compact_when_messages_shorter_than_keep_recent() {
        let msgs = vec![
            ChatMessage::System("sys".into()),
            make_tool_result("c1", "x"),
        ];
        let cfg = MicrocompactConfig {
            trigger_tokens: 1,
            keep_recent: 100,
            ..Default::default()
        };
        let (_, r) = microcompact(msgs, &cfg);
        assert!(!r.was_compacted);
    }
}
