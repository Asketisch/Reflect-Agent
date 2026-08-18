//! `ChatRequest` 及其嵌套类型。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use reflect_protocol::ToolOutput;

// ── 顶层请求 ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 模型名,**不**含 provider 前缀(如 `"gpt-4o"`、`"claude-3-5-sonnet-latest"`)。
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub system: SystemBlocks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub cache_control: Vec<CacheBreak>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub stop: Vec<String>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            thinking: None,
            cache_control: Vec::new(),
            metadata: HashMap::new(),
            stop: Vec::new(),
        }
    }
}

// ── 消息 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System(String),
    User(UserContent),
    Assistant(AssistantContent),
    Tool(ToolResult),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserContent {
    pub blocks: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    /// 完整缓冲的 JSON 参数(由多个 delta 拼接而成)。
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    /// 工具输出的类型化内容块。承载 `Text` 与 `Image`(多模态)块,
    /// 让支持图片工具结果(Anthropic)的 provider 能以可查看图片的形式
    /// 暴露给模型,而不是压平成字节数组字符串。此前本字段是 `String`,
    /// 通过 JSON 序列化整个 content 数组得到,这会破坏图片内容
    /// (`[{"type":"image","data":[137,80,...]}]` 文本)—— 是所有"图片查看"
    /// 类问题失败的根因(GAIA chess 等)。
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn from_output(call_id: impl Into<String>, output: &ToolOutput) -> Self {
        // 将协议层 ContentBlock 映射到 LLM 层 ContentBlock,
        // 保留 Image 块(多模态),而非压平成 JSON 字符串。
        // 非多模态的协议变体(Diff / ToolUse / 嵌套 ToolResult)降级为文本表示。
        let content = output
            .content
            .iter()
            .map(|b| match b {
                reflect_protocol::ContentBlock::Text { text } => {
                    ContentBlock::Text { text: text.clone() }
                }
                reflect_protocol::ContentBlock::Image { data, mime_type } => ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                },
                // 非多模态 / 结构化变体:序列化为字符串,让模型仍能看到内容
                // (这些变体从不承载可查看图片)。
                other => ContentBlock::Text {
                    text: serde_json::to_string(other).unwrap_or_default(),
                },
            })
            .collect();
        Self {
            call_id: call_id.into(),
            content,
            is_error: output.is_error,
        }
    }

    /// 将内容块压平为纯文本字符串,供需要单字符串的 provider / 消费者使用
    /// (token 估算、summarizer、OpenAI / Ollama 只接受纯文本的
    /// tool-result `content` 字段)。Image 块渲染为短占位(绝不输出原始字节)。
    pub fn content_as_text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            match b {
                ContentBlock::Text { text } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
                ContentBlock::Image { data, mime_type } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("[image: {mime_type}, {} bytes]", data.len()));
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: Vec<u8>, mime_type: String },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

// ── system 块 + 缓存控制 ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemBlocks(pub Vec<SystemBlock>);

impl SystemBlocks {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|b| b.text.is_empty())
    }
    /// 将非空 system 合并为单字符串(OpenAI 没有 system blocks;Anthropic 有)。
    pub fn as_single_string(&self) -> Option<String> {
        let parts: Vec<&str> = self.0.iter().map(|b| b.text.as_str()).collect();
        let joined = parts.join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

impl Default for CacheControl {
    fn default() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: Some(CacheTtl::FiveMinutes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheBreak {
    pub after_message_index: usize,
    pub ttl: CacheTtl,
}

// ── 思考配置 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Enabled { budget_tokens: u32 },
    Disabled,
    OpenAIReasoning { effort: ReasoningEffort },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// v1.x S4:把协议层 `ReasoningEffortMirror` 桥接到 LLM 层 `ReasoningEffort`。
///
/// 协议层独立枚举是为了避免 `protocol → llm` 反向依赖;`submission_loop`
/// / `model_call` 是**唯一**需要该转换的地方,集中在 `From` impl 便于
/// 协议字段扩展(加 `XHigh` 等)时单点同步。
impl From<reflect_protocol::ReasoningEffortMirror> for ReasoningEffort {
    fn from(m: reflect_protocol::ReasoningEffortMirror) -> Self {
        match m {
            reflect_protocol::ReasoningEffortMirror::Low => ReasoningEffort::Low,
            reflect_protocol::ReasoningEffortMirror::Medium => ReasoningEffort::Medium,
            reflect_protocol::ReasoningEffortMirror::High => ReasoningEffort::High,
        }
    }
}

// ── Tool spec(供 LLM 层使用的 reflect_tools::ToolSpec 镜像)─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

impl ToolSpec {
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Function { name, .. } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_serde_roundtrip() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            })],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: Some(0.7),
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: HashMap::new(),
            stop: vec![],
        };
        let j = serde_json::to_string(&req).unwrap();
        let back: ChatRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.model, "gpt-4o");
        assert_eq!(back.messages.len(), 1);
    }

    #[test]
    fn system_blocks_as_single_string_skips_empty() {
        let blocks = SystemBlocks(vec![SystemBlock {
            text: "x".into(),
            cache_control: None,
            ephemeral: false,
        }]);
        assert_eq!(blocks.as_single_string().as_deref(), Some("x"));
        assert!(SystemBlocks::default().as_single_string().is_none());
    }

    /// GAIA-fix:`from_output` 必须把 protocol 层的 `Image` 块**保留**为 LLM 层的
    /// `ContentBlock::Image`(而非 JSON 序列化成字节数组字符串),这样 provider
    /// 才能把图片以 base64 形式发给模型。
    #[test]
    fn from_output_preserves_image_block() {
        let output = reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Image {
                data: vec![0x89, 0x50, 0x4e, 0x47],
                mime_type: "image/png".into(),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        };
        let r = ToolResult::from_output("c1", &output);
        assert_eq!(r.content.len(), 1);
        match &r.content[0] {
            ContentBlock::Image { data, mime_type } => {
                assert_eq!(data, &vec![0x89, 0x50, 0x4e, 0x47]);
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected Image block, got {other:?}"),
        }
    }

    /// `from_output` 文本块映射为 LLM 层 Text 块(回归保护)。
    #[test]
    fn from_output_maps_text_block() {
        let output = reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text {
                text: "hello".into(),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        };
        let r = ToolResult::from_output("c1", &output);
        assert_eq!(r.content_as_text(), "hello");
    }

    /// `content_as_text`:混合 Text+Image 块时,Image 渲染为短占位(非原始字节)。
    #[test]
    fn content_as_text_image_placeholder() {
        let r = ToolResult {
            call_id: "c1".into(),
            content: vec![
                ContentBlock::text("a board state"),
                ContentBlock::Image {
                    data: vec![1; 5000],
                    mime_type: "image/png".into(),
                },
            ],
            is_error: false,
        };
        let text = r.content_as_text();
        assert!(text.contains("a board state"));
        assert!(text.contains("[image: image/png, 5000 bytes]"));
        // 关键:绝不能把字节数组灌进文本。
        assert!(!text.contains("[1, 1, 1"));
    }
}
