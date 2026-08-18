//! Insta snapshot tests for provider request serialization.

use reflect_llm::providers::anthropic::AnthropicRequest;
use reflect_llm::providers::openai::OpenAIRequest;
use reflect_llm::request::{SystemBlock, SystemBlocks, ThinkingConfig};
use reflect_llm::{
    CacheControl, CacheControlKind, CacheTtl, ChatMessage, ChatRequest, ContentBlock,
    ReasoningEffort, SystemBlocks as SBP, UserContent,
};
use serde_json::Value;

fn basic_request() -> ChatRequest {
    ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })],
        tools: vec![],
        system: SBP::default(),
        temperature: None,
        max_tokens: Some(256),
        top_p: None,
        thinking: Some(ThinkingConfig::OpenAIReasoning {
            effort: ReasoningEffort::Medium,
        }),
        cache_control: vec![],
        metadata: Default::default(),
        stop: vec![],
    }
}

#[test]
fn snapshot_openai_request_basic() {
    let v: Value = serde_json::to_value(OpenAIRequest::from(basic_request())).unwrap();
    insta::assert_yaml_snapshot!(v);
}

#[test]
fn snapshot_anthropic_request_with_caching_and_thinking() {
    let mut req = basic_request();
    req.system = SystemBlocks(vec![SystemBlock {
        text: "you are a helpful assistant".into(),
        cache_control: None,
        ephemeral: false,
    }]);
    req.thinking = Some(ThinkingConfig::Enabled {
        budget_tokens: 1024,
    });
    let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    insta::assert_yaml_snapshot!(v);
}

#[test]
fn snapshot_anthropic_request_with_existing_cache_control() {
    let mut req = basic_request();
    req.system = SystemBlocks(vec![SystemBlock {
        text: "1h-cached".into(),
        cache_control: Some(CacheControl {
            kind: CacheControlKind::Ephemeral,
            ttl: Some(CacheTtl::OneHour),
        }),
        ephemeral: false,
    }]);
    let v: Value = serde_json::to_value(AnthropicRequest::from(req)).unwrap();
    insta::assert_yaml_snapshot!(v);
}
