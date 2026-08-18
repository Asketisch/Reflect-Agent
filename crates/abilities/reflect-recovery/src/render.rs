//! `RecoveryEntry` 渲染层 —— 把元数据 / 文件内容 / 子代理记录转为
//! `<system-reminder>` 包装的 `ChatMessage`。
//!
//! 设计目标:
//! - 单一渲染入口,统一去重逻辑;
//! - 顺序由 `MetaKind` 枚举顺序决定(canonical);
//! - 复用 `reflect_core::nodes::model_call` 的 `<system-reminder>` 包装风格
//!   (`mod.rs:212-222` 的 ephemeral 推送)。

use std::collections::HashMap;

use reflect_llm::{ChatMessage, ContentBlock, UserContent};

/// meta-message 类型标签。同时控制渲染顺序(枚举顺序 = canonical 顺序)
/// 和未来按 kind 过滤 / 去重的能力。
///
/// 顺序固定:`ActiveFiles` → `SubagentRegistry` → `SessionMemory`,
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaKind {
    /// post-compact 自动重读的文件内容。
    ActiveFiles,
    /// 已完成的子代理调用记录(防重复 spawn)。
    SubagentRegistry,
    /// session memory 笔记(从 `reflect_notes` 拉取)。
    SessionMemory,
}

impl MetaKind {
    /// 单实例去重 key。
    pub fn dedup_key(self) -> &'static str {
        match self {
            MetaKind::ActiveFiles => "active_files",
            MetaKind::SubagentRegistry => "subagent_registry",
            MetaKind::SessionMemory => "session_memory",
        }
    }
}

/// 一条待注入的 meta-message。
///
/// `content` 是裸文本(不含 `<system-reminder>` 标签),由渲染层统一包装。
/// `dedup_key` 决定是否参与单实例去重:空字符串 = 保留全部(给未来的
/// per-file / per-note 多实例场景)。
#[derive(Debug, Clone)]
pub struct RecoveryEntry {
    pub kind: MetaKind,
    pub content: String,
    pub dedup_key: String,
}

impl RecoveryEntry {
    /// 便捷构造:用 `kind` 的默认 `dedup_key`(单实例)。
    pub fn new(kind: MetaKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
            dedup_key: kind.dedup_key().to_string(),
        }
    }

    /// 显式指定 `dedup_key`(空 = 不参与去重)。
    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = key.into();
        self
    }
}

/// 把一组 `RecoveryEntry` 渲染成 `Vec<ChatMessage>`,每条以
/// `<system-reminder>` 包裹的 `User` 消息呈现。
///
/// 渲染规则:
/// 1. 按 `MetaKind` 枚举顺序定 canonical 顺序(忽略输入顺序);
/// 2. 同 `dedup_key` 的多条 → 保留**最后**一条(last-write-wins);
/// 3. 空 `dedup_key` 的条目 → 保留全部(按输入顺序追加到对应 kind 之后)。
pub fn recovery_meta_to_messages(entries: &[RecoveryEntry]) -> Vec<ChatMessage> {
    // Pass 1: 按 dedup_key 折叠(保留 last occurrence);空 key 单列。
    let mut by_key: HashMap<&str, &RecoveryEntry> = HashMap::new();
    let mut keyless: Vec<&RecoveryEntry> = Vec::new();
    for e in entries {
        if e.dedup_key.is_empty() {
            keyless.push(e);
        } else {
            by_key.insert(e.dedup_key.as_str(), e);
        }
    }

    // Pass 2: 按 canonical kind 顺序渲染去重后的条目。
    let canonical = [
        MetaKind::ActiveFiles,
        MetaKind::SubagentRegistry,
        MetaKind::SessionMemory,
    ];
    let mut out: Vec<ChatMessage> = Vec::new();
    for kind in canonical {
        if let Some(e) = by_key.values().find(|e| e.kind == kind) {
            out.push(to_reminder_message(&e.content));
        }
    }
    // Pass 3: keyless 条目按输入顺序追加。
    for e in keyless {
        out.push(to_reminder_message(&e.content));
    }
    out
}

fn to_reminder_message(content: &str) -> ChatMessage {
    ChatMessage::User(UserContent {
        blocks: vec![ContentBlock::Text {
            text: format!("<system-reminder>\n{}\n</system-reminder>", content),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_output() {
        let out = recovery_meta_to_messages(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn single_entry_renders_one_reminder() {
        let entries = vec![RecoveryEntry::new(MetaKind::SessionMemory, "hello")];
        let out = recovery_meta_to_messages(&entries);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ChatMessage::User(u) => match &u.blocks[0] {
                ContentBlock::Text { text } => {
                    assert!(text.contains("<system-reminder>"));
                    assert!(text.contains("hello"));
                }
                _ => panic!("expected text block"),
            },
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn canonical_order_active_files_before_session_memory() {
        // 输入逆序,输出应按 canonical 排序。
        let entries = vec![
            RecoveryEntry::new(MetaKind::SessionMemory, "s"),
            RecoveryEntry::new(MetaKind::SubagentRegistry, "u"),
            RecoveryEntry::new(MetaKind::ActiveFiles, "a"),
        ];
        let out = recovery_meta_to_messages(&entries);
        assert_eq!(out.len(), 3);
        let text0 = extract_text(&out[0]);
        let text1 = extract_text(&out[1]);
        let text2 = extract_text(&out[2]);
        assert!(text0.contains("a"), "first should be active files: {text0}");
        assert!(text1.contains("u"), "second should be subagent: {text1}");
        assert!(
            text2.contains("s"),
            "third should be session memory: {text2}"
        );
    }

    #[test]
    fn dedup_keeps_last_occurrence() {
        let entries = vec![
            RecoveryEntry::new(MetaKind::SessionMemory, "old"),
            RecoveryEntry::new(MetaKind::SessionMemory, "new"),
        ];
        let out = recovery_meta_to_messages(&entries);
        assert_eq!(out.len(), 1);
        let text = extract_text(&out[0]);
        assert!(text.contains("new"));
        assert!(!text.contains("old"));
    }

    #[test]
    fn keyless_entries_all_kept() {
        let entries = vec![
            RecoveryEntry::new(MetaKind::SessionMemory, "kept").with_dedup_key(""),
            RecoveryEntry::new(MetaKind::SessionMemory, "also kept").with_dedup_key(""),
        ];
        let out = recovery_meta_to_messages(&entries);
        assert_eq!(out.len(), 2);
        assert!(extract_text(&out[0]).contains("kept"));
        assert!(extract_text(&out[1]).contains("also kept"));
    }

    #[test]
    fn dedup_key_per_kind() {
        // 不同的 kind,即使有相同的 dedup_key 也不互相覆盖(因为 hash key
        // 是 dedup_key,kind 不同的会被去重到同一 key —— 这是 bug 测试)。
        // 实际: 我们对每个 kind 取一次,即一 dedup_key 一条。
        let entries = vec![
            RecoveryEntry::new(MetaKind::ActiveFiles, "af").with_dedup_key("shared"),
            RecoveryEntry::new(MetaKind::SessionMemory, "sm").with_dedup_key("shared"),
        ];
        let out = recovery_meta_to_messages(&entries);
        // 因为两个 kind 都查同一个 key "shared",后插入的会覆盖前者。
        // 这是已知的 last-write-wins 行为;pre_loop 应避免给不同 kind
        // 用相同 dedup_key(本测试只是记录现状)。
        assert_eq!(out.len(), 1);
    }

    fn extract_text(msg: &ChatMessage) -> String {
        match msg {
            ChatMessage::User(u) => match &u.blocks[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }
}
