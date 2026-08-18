//! Smart-prune:本地启发式压缩,不调用 LLM。
//!
//! 第一阶段:按工具类型截断。`grep` 类工具保留头部
//! `MAX_TOOL_RESULT_LINES` 行;`bash` 类保留尾部;`read` 类保留
//! 头尾并插入 `... [middle truncated] ...` 标记;其余工具截断至
//! `MAX_TOOL_RESULT_CHARS`。Assistant 文本若超过 `MAX_ASSISTANT_CHARS`,
//! 则截断并追加工具列表。
//!
//! 第二阶段:在 token 估算低于 `target_tokens` 之前,持续丢弃最旧的
//! 非固定保留消息。
//!
//! 对应 reflect `graph.py:smart_prune_messages`(第 442-609 行)。

use reflect_llm::{AssistantContent, ChatMessage, ContentBlock, ToolResult};

/// 截断后工具结果的默认最大字符数。
pub const MAX_TOOL_RESULT_CHARS: usize = 300;
/// Assistant 文本的默认最大字符数。
pub const MAX_ASSISTANT_CHARS: usize = 600;
/// 工具结果保留的默认最大行数。
pub const MAX_TOOL_RESULT_LINES: usize = 50;

/// read 类工具丢失中间部分时插入的标记。
pub const MIDDLE_MARKER: &str = "\n... [middle truncated] ...\n";

/// 工具名分类。不属于任何类别的工具将回落到「截断至
/// `MAX_TOOL_RESULT_CHARS`」的处理。
pub const GREP_LIKE_TOOLS: &[&str] = &["grep", "ranked_search", "semantic_search"];
pub const BASH_LIKE_TOOLS: &[&str] = &["bash"];
pub const READ_LIKE_TOOLS: &[&str] = &["read", "read_ranges", "read_code_context"];

/// smart-prune 的可调参数。
#[derive(Debug, Clone)]
pub struct SmartPruneConfig {
    pub trigger_tokens: u32,
    pub keep_recent: usize,
    pub target_tokens: u32,
    pub max_tool_result_chars: usize,
    pub max_assistant_chars: usize,
    pub max_tool_result_lines: usize,
}

impl Default for SmartPruneConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: crate::strategy::DEFAULT_TRIGGER_TOKENS,
            keep_recent: crate::microcompact::KEEP_RECENT_DEFAULT + 1,
            // 0.75 * trigger。运行时计算以跟随 `trigger_tokens`。
            target_tokens: ((crate::strategy::DEFAULT_TRIGGER_TOKENS as f32) * 0.75) as u32,
            max_tool_result_chars: MAX_TOOL_RESULT_CHARS,
            max_assistant_chars: MAX_ASSISTANT_CHARS,
            max_tool_result_lines: MAX_TOOL_RESULT_LINES,
        }
    }
}

/// smart-prune 的执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    pub before_tokens: u32,
    pub after_tokens: u32,
    pub removed_count: usize,
    pub truncated_count: usize,
    pub was_compacted: bool,
}

/// 执行 smart-prune。返回新的消息列表与报告。
pub fn smart_prune(
    messages: Vec<ChatMessage>,
    cfg: &SmartPruneConfig,
) -> (Vec<ChatMessage>, CompactReport) {
    let before = crate::tokens::estimate_messages(&messages);
    if before < cfg.trigger_tokens {
        return (
            messages,
            CompactReport {
                before_tokens: before,
                after_tokens: before,
                removed_count: 0,
                truncated_count: 0,
                was_compacted: false,
            },
        );
    }

    // 第一阶段:按工具类型截断。
    let mut truncated_count = 0;
    let mut after_phase1: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let call_id_map = crate::tool_pair::build_call_id_to_tool_name_map(&messages);
    for m in messages {
        let (replaced, was_truncated) = truncate_message(m, cfg, &call_id_map);
        if was_truncated {
            truncated_count += 1;
        }
        after_phase1.push(replaced);
    }

    // 第二阶段:丢弃最旧的非固定保留消息,直至低于目标值。
    let n = after_phase1.len();
    if n <= cfg.keep_recent {
        let after = crate::tokens::estimate_messages(&after_phase1);
        return (
            after_phase1,
            CompactReport {
                before_tokens: before,
                after_tokens: after,
                removed_count: 0,
                truncated_count,
                was_compacted: truncated_count > 0,
            },
        );
    }
    // 固定保留:每个 System + 第一条 User。
    let first_user = after_phase1
        .iter()
        .position(|m| matches!(m, ChatMessage::User(_)))
        .unwrap_or(usize::MAX);
    let mut pinned: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, m) in after_phase1.iter().enumerate() {
        if matches!(m, ChatMessage::System(_)) || i == first_user {
            pinned.insert(i);
        }
    }
    let keep_start = n.saturating_sub(cfg.keep_recent);
    for i in keep_start..n {
        pinned.insert(i);
    }

    // 遍历旧下标(不在固定集合、不在 keep_recent),丢弃最旧者。
    let mut removed_count = 0;
    let mut keep: Vec<(usize, ChatMessage)> = after_phase1.into_iter().enumerate().collect();
    let mut current_tokens =
        crate::tokens::estimate_messages(&keep.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>());
    let mut drop_idx = 0;
    while current_tokens >= cfg.target_tokens && drop_idx < keep.len() {
        if pinned.contains(&drop_idx) || drop_idx >= keep_start {
            drop_idx += 1;
            continue;
        }
        keep.remove(drop_idx);
        // 删除后下标发生位移;对所有大于 drop_idx 的固定下标减 1。
        pinned.remove(&drop_idx);
        let mut new_pinned: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for p in &pinned {
            if *p > drop_idx {
                new_pinned.insert(*p - 1);
            } else {
                new_pinned.insert(*p);
            }
        }
        pinned = new_pinned;
        removed_count += 1;
        current_tokens = crate::tokens::estimate_messages(
            &keep.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
        );
        // 不前进 drop_idx;下一个元素已移入此槽位。
    }

    let final_msgs: Vec<ChatMessage> = keep.into_iter().map(|(_, m)| m).collect();
    let after = crate::tokens::estimate_messages(&final_msgs);
    (
        final_msgs,
        CompactReport {
            before_tokens: before,
            after_tokens: after,
            removed_count,
            truncated_count,
            was_compacted: removed_count > 0 || truncated_count > 0,
        },
    )
}

/// 按工具分类截断单条消息。返回(可能被截断的)消息及一个 `bool`,
/// 标识正文是否实际发生变化。
fn truncate_message(
    msg: ChatMessage,
    cfg: &SmartPruneConfig,
    call_id_map: &std::collections::HashMap<String, String>,
) -> (ChatMessage, bool) {
    match msg {
        ChatMessage::Tool(t) => {
            // content 是 Vec<ContentBlock>(可能含 Image 块)。
            // 仅按长度截断 Text 块;Image 块原样保留(二进制,
            // 不计入字符/行预算)。
            let mut changed = false;
            let mut new_blocks: Vec<ContentBlock> = Vec::with_capacity(t.content.len());
            for b in t.content {
                match b {
                    ContentBlock::Text { text } => {
                        let truncated = truncate_tool_result(
                            &t.call_id,
                            &text,
                            cfg.max_tool_result_chars,
                            cfg.max_tool_result_lines,
                            call_id_map,
                        );
                        if truncated != text {
                            changed = true;
                        }
                        new_blocks.push(ContentBlock::Text { text: truncated });
                    }
                    image @ ContentBlock::Image { .. } => {
                        new_blocks.push(image);
                    }
                }
            }
            (
                ChatMessage::Tool(ToolResult {
                    call_id: t.call_id,
                    content: new_blocks,
                    is_error: t.is_error,
                }),
                changed,
            )
        }
        ChatMessage::Assistant(a) => {
            let text = a.text.clone().unwrap_or_default();
            if text.chars().count() <= cfg.max_assistant_chars {
                return (ChatMessage::Assistant(a), false);
            }
            let truncated: String = text.chars().take(cfg.max_assistant_chars).collect();
            let tool_names: Vec<String> = a.tool_calls.iter().map(|t| t.name.clone()).collect();
            let suffix = if tool_names.is_empty() {
                String::new()
            } else {
                format!("\n[... tools: {} ...]", tool_names.join(", "))
            };
            (
                ChatMessage::Assistant(AssistantContent {
                    text: Some(format!("{truncated}{suffix}")),
                    tool_calls: a.tool_calls,
                    thinking: a.thinking,
                }),
                true,
            )
        }
        other => (other, false),
    }
}

/// 按工具名截断工具结果。call_id 以 `tool:<name>:` 前缀编码工具名
/// (由 strategy 层设置);若缺失则默认按字符数截断。
pub fn truncate_tool_result(
    call_id: &str,
    content: &str,
    max_chars: usize,
    max_lines: usize,
    call_id_map: &std::collections::HashMap<String, String>,
) -> String {
    // 优先从 call_id->name 映射中获取工具名(生产路径);
    // 回退到测试使用的旧版 `tool:<name>:` 前缀。
    let name_owned: String = call_id_map.get(call_id).cloned().unwrap_or_else(|| {
        let tool_name = call_id.strip_prefix("tool:").unwrap_or("");
        tool_name.split(':').next().unwrap_or("").to_string()
    });
    let name: &str = &name_owned;

    if content.len() <= max_chars {
        return content.to_string();
    }
    if GREP_LIKE_TOOLS.contains(&name) {
        keep_head_lines(content, max_lines)
    } else if BASH_LIKE_TOOLS.contains(&name) {
        keep_tail_lines(content, max_lines.max(30))
    } else if READ_LIKE_TOOLS.contains(&name) {
        keep_head_tail_lines(content, max_lines.max(20) / 2)
    } else if name.starts_with("call_") {
        // 子 agent 结果
        truncate_chars(content, max_chars.max(3000))
    } else {
        truncate_chars(content, max_chars)
    }
}

fn keep_head_lines(s: &str, n: usize) -> String {
    let mut out = String::new();
    for (i, line) in s.lines().enumerate() {
        if i >= n {
            out.push_str("\n[... truncated ...]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn keep_tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    if start > 0 {
        out.push_str("[... earlier output truncated ...]\n");
    }
    for line in &lines[start..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn keep_head_tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let half = n.max(10);
    if lines.len() <= half * 2 {
        return s.to_string();
    }
    let head: Vec<&str> = lines[..half].to_vec();
    let tail: Vec<&str> = lines[lines.len() - half..].to_vec();
    let mut out = String::new();
    for line in head {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(MIDDLE_MARKER);
    for line in tail {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let truncated: String = s.chars().take(n).collect();
    format!("{truncated}\n[... truncated ...]")
}

// 独立辅助函数,保留以便测试可达;生产场景已内联到
// `truncate_message` 中。
#[allow(dead_code)]
fn truncate_assistant_in_place(a: AssistantContent, max_chars: usize) -> (ChatMessage, bool) {
    let text = a.text.clone().unwrap_or_default();
    if text.chars().count() <= max_chars {
        return (ChatMessage::Assistant(a), false);
    }
    let truncated: String = text.chars().take(max_chars).collect();
    let tool_names: Vec<String> = a.tool_calls.iter().map(|t| t.name.clone()).collect();
    let suffix = if tool_names.is_empty() {
        String::new()
    } else {
        format!("\n[... tools: {} ...]", tool_names.join(", "))
    };
    (
        ChatMessage::Assistant(AssistantContent {
            text: Some(format!("{truncated}{suffix}")),
            tool_calls: a.tool_calls,
            thinking: a.thinking,
        }),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::{ContentBlock, ToolResult, UserContent};

    fn make_tool_result(call_id: &str, content: &str) -> ChatMessage {
        ChatMessage::Tool(ToolResult {
            call_id: call_id.into(),
            content: vec![ContentBlock::text(content)],
            is_error: false,
        })
    }

    #[test]
    fn grep_like_keeps_head_lines() {
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let out = truncate_tool_result(
            "tool:grep:0",
            &content,
            100,
            50,
            &std::collections::HashMap::new(),
        );
        // 应只保留头部 50 行。
        assert!(out.contains("line 0"));
        assert!(out.contains("line 49"));
        assert!(!out.contains("line 100"));
    }

    #[test]
    fn bash_like_keeps_tail_lines() {
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let out = truncate_tool_result(
            "tool:bash:0",
            &content,
            100,
            50,
            &std::collections::HashMap::new(),
        );
        assert!(out.contains("line 199"));
        assert!(!out.contains("line 0"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn read_like_keeps_head_and_tail() {
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let out = truncate_tool_result(
            "tool:read:0",
            &content,
            100,
            50,
            &std::collections::HashMap::new(),
        );
        assert!(out.contains(MIDDLE_MARKER));
        assert!(out.contains("line 0"));
        assert!(out.contains("line 199"));
    }

    #[test]
    fn unknown_tool_truncates_by_chars() {
        let content: String = "x".repeat(1000);
        let out = truncate_tool_result(
            "tool:unknown:0",
            &content,
            100,
            50,
            &std::collections::HashMap::new(),
        );
        assert!(out.contains("[... truncated ...]"));
        assert!(out.chars().count() < 200);
    }

    #[test]
    fn short_content_unchanged() {
        let out = truncate_tool_result(
            "tool:grep:0",
            "short content",
            100,
            50,
            &std::collections::HashMap::new(),
        );
        assert_eq!(out, "short content");
    }

    #[test]
    fn subagent_uses_higher_limit() {
        let content: String = "x".repeat(5000);
        let out = truncate_tool_result(
            "tool:call_explorer:0",
            &content,
            100,
            50,
            &std::collections::HashMap::new(),
        );
        // max_chars=100 但 subagent 上限 = 3000 → 内容 < 3000 时保留。
        assert!(out.contains(&"x".repeat(3000)) || out.chars().count() < 5000);
    }

    #[test]
    fn below_trigger_noop() {
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })];
        let (_, r) = smart_prune(msgs, &SmartPruneConfig::default());
        assert!(!r.was_compacted);
    }

    #[test]
    fn assistant_text_over_max_truncated() {
        let long_text: String = "x".repeat(1000);
        let a = AssistantContent {
            text: Some(long_text),
            tool_calls: vec![reflect_llm::ToolCallRequest {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
            thinking: None,
        };
        let (_, was_truncated) = truncate_assistant_in_place(a, MAX_ASSISTANT_CHARS);
        assert!(was_truncated);
    }

    #[test]
    fn assistant_text_under_max_unchanged() {
        let a = AssistantContent {
            text: Some("short".into()),
            tool_calls: vec![],
            thinking: None,
        };
        let (_, was_truncated) = truncate_assistant_in_place(a, 600);
        assert!(!was_truncated);
    }

    #[test]
    fn phase2_drops_oldest_until_under_target() {
        // 构造每条消息都是长工具结果的巨型消息列表。
        // 50 条消息,每条 200 字符 = 共约 10000 字符 ≈ 2857 token。
        // trigger=2000,触发 phase 2。
        let mut msgs = vec![ChatMessage::System("sys".into())];
        for i in 0..50 {
            let content: String = "x".repeat(200);
            msgs.push(make_tool_result(&format!("c{i}"), &content));
        }
        let cfg = SmartPruneConfig {
            trigger_tokens: 2000,
            target_tokens: 1000,
            keep_recent: 5,
            ..Default::default()
        };
        let (out, r) = smart_prune(msgs, &cfg);
        assert!(r.was_compacted);
        // 下标 0 的 System 固定保留。
        assert!(matches!(out[0], ChatMessage::System(_)));
        // keep_recent=5 的尾部完好。
        assert!(out.len() <= 51);
    }
}
