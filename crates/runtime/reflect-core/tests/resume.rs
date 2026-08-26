//! `resume::records_to_preload` — RolloutRecord → ChatMessage 映射的回归测试。
//!
//! 覆盖:legacy String 形态、图文 blocks 形态(user 侧 ToolUse 忽略)、
//! assistant 工具对重建顺序(Assistant 在前 / Tool 紧随)、compaction 摘要
//! 前置 System、审计记录忽略、空输入。

use reflect_core::resume::records_to_preload;
use reflect_llm::ChatMessage;
use reflect_protocol::{ContentBlock, MessageRole, RolloutRecord, ToolOutput, TurnId};

fn msg(role: MessageRole, content: serde_json::Value) -> RolloutRecord {
    RolloutRecord::message(TurnId::new(), role, content)
}

fn tool_output(text: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text { text: text.into() }],
        is_error: false,
        metadata: serde_json::json!({}),
        elapsed_ms: 1,
    }
}

#[test]
fn legacy_string_user_and_assistant() {
    let records = vec![
        msg(MessageRole::User, serde_json::json!("我叫 Alice")),
        msg(MessageRole::Assistant, serde_json::json!("你好 Alice")),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 2);
    assert!(matches!(&out[0], ChatMessage::User(u) if u.blocks.len() == 1));
    assert!(
        matches!(&out[1], ChatMessage::Assistant(a) if a.text.as_deref() == Some("你好 Alice"))
    );
}

#[test]
fn user_block_array_keeps_text_and_image_skips_tooluse() {
    let blocks = vec![
        ContentBlock::Text {
            text: "看这张图".into(),
        },
        ContentBlock::Image {
            data: vec![1, 2, 3],
            mime_type: "image/png".into(),
        },
        // user 侧不应出现 ToolUse;出现则忽略。
        ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
        },
    ];
    let records = vec![msg(
        MessageRole::User,
        serde_json::to_value(&blocks).unwrap(),
    )];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 1);
    match &out[0] {
        ChatMessage::User(u) => {
            assert_eq!(u.blocks.len(), 2, "Text + Image 保留,ToolUse 忽略");
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn assistant_tool_pair_rebuilt_in_order() {
    let blocks = vec![
        ContentBlock::Text {
            text: "我去查一下".into(),
        },
        ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "grep".into(),
            args: serde_json::json!({"q": "foo"}),
        },
        ContentBlock::ToolResult {
            call_id: "call-1".into(),
            output: tool_output("3 hits"),
        },
    ];
    let records = vec![msg(
        MessageRole::Assistant,
        serde_json::to_value(&blocks).unwrap(),
    )];
    let out = records_to_preload(&records);
    // Assistant(含 tool_calls)在前,Tool(ToolResult)紧随其后。
    assert_eq!(out.len(), 2);
    match &out[0] {
        ChatMessage::Assistant(a) => {
            assert_eq!(a.text.as_deref(), Some("我去查一下"));
            assert_eq!(a.tool_calls.len(), 1);
            assert_eq!(a.tool_calls[0].id, "call-1");
            assert_eq!(a.tool_calls[0].name, "grep");
        }
        other => panic!("expected Assistant first, got {other:?}"),
    }
    match &out[1] {
        ChatMessage::Tool(t) => assert_eq!(t.call_id, "call-1"),
        other => panic!("expected Tool second, got {other:?}"),
    }
}

#[test]
fn compaction_summary_becomes_leading_system() {
    let records = vec![
        msg(MessageRole::User, serde_json::json!("旧问题")),
        RolloutRecord::Compaction {
            turn_id: TurnId::new(),
            strategy: "microcompact".into(),
            removed_count: 4,
            summary: "会话摘要:讨论了 X".into(),
        },
        // 后续消息保留(全量历史 + 摘要前置是既定语义)。
        msg(MessageRole::User, serde_json::json!("新问题")),
    ];
    let out = records_to_preload(&records);
    // System(摘要) + 旧 user + 新 user。
    assert_eq!(out.len(), 3);
    assert!(matches!(&out[0], ChatMessage::System(s) if s.contains("会话摘要")));
}

#[test]
fn audit_records_ignored() {
    let records = vec![
        RolloutRecord::SessionMeta {
            session_id: reflect_protocol::ThreadId::new(),
            model: "m".into(),
            started_at: chrono::Utc::now(),
            workspace: None,
        },
        RolloutRecord::Checkpoint {
            turn_id: TurnId::new(),
            sha: "abc".into(),
            label: None,
            created_at: chrono::Utc::now(),
        },
        RolloutRecord::TokenCount {
            turn_id: TurnId::new(),
            usage: Default::default(),
            cost_usd: None,
            at: chrono::Utc::now(),
        },
        msg(MessageRole::User, serde_json::json!("唯一有效消息")),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 1, "仅对话消息参与,审计轨迹忽略");
}

#[test]
fn empty_input_empty_output() {
    assert!(records_to_preload(&[]).is_empty());
}
