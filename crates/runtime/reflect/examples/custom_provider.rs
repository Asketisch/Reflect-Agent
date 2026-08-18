//! `custom_provider` —— 注册第三个 `ModelClient` 实现。
//!
//! 本示例使用 `MockLlmClient`,流式输出固定字符串,完全不发任何网络请求。
//! 适合作为离线测试模板,或集成自定义推理后端的样例。
//!
//! 运行:
//! ```bash
//! cargo run -p reflect --example custom_provider
//! ```

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use reflect::{
    AnthropicClient, AnthropicConfig, Capabilities, ChatEvent, ChatRequest, CredentialPool,
    EventMsg, LlmError, ModelClient, ModelRegistry, OllamaClient, OllamaConfig, OpenAIClient,
    OpenAIConfig, PoolEntry, Reflect, Submission,
};
use tokio_util::sync::CancellationToken;

struct MockLlmClient {
    response: String,
}

#[async_trait]
impl ModelClient for MockLlmClient {
    fn name(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: false,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
    {
        let response = self.response.clone();
        let chunks: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::MessageStart {
                id: "msg_mock".into(),
                model: "mock-1".into(),
            }),
            Ok(ChatEvent::ContentDelta("Hello ".into())),
            Ok(ChatEvent::ContentDelta("from ".into())),
            Ok(ChatEvent::ContentDelta("mock ".into())),
            Ok(ChatEvent::ContentDelta("LLM!".into())),
            Ok(ChatEvent::Usage {
                input_tokens: 0,
                output_tokens: 4,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            Ok(ChatEvent::MessageStop),
        ];
        // 健全性检查 response(留作未来使用;同时消除未使用警告)。
        let _ = response;
        Ok(Box::pin(stream::iter(chunks)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 构造一个注册表,同时包含真实 provider(便于切换使用)与 mock —— 在
    // 无网络环境下迭代 agent 逻辑时很有用。
    let registry = Arc::new(ModelRegistry::new());
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.is_empty()
    {
        registry.register_pool(
            "openai",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(OpenAIClient::new(OpenAIConfig {
                        api_key: key,
                        ..Default::default()
                    })?),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        registry.register_pool(
            "anthropic",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(AnthropicClient::new(AnthropicConfig {
                        api_key: key,
                        ..Default::default()
                    })?),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
    }
    // v0.3.1: Ollama —— 用 `OLLAMA_HOST` 指向非默认 server;本地默认
    // 127.0.0.1:11434 时仍需显式 set env 才能注册(避免示例运行找不到
    // server 时拿到 confusing 错误)。
    let ollama_host = std::env::var("OLLAMA_HOST").ok();
    let ollama_key = std::env::var("OLLAMA_API_KEY").ok();
    if ollama_host.is_some() || ollama_key.is_some() {
        registry.register_pool(
            "ollama",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(OllamaClient::new(OllamaConfig {
                        base_url: ollama_host,
                        api_key: ollama_key,
                        ..Default::default()
                    })?),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
    }
    let _ = registry; // suppress unused warning
    registry.register_pool(
        "mock",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(MockLlmClient {
                    response: "Hello from mock LLM!".into(),
                }),
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    // 手动构造 agent 以便传入自定义 registry。
    use reflect_core::{AgentConfig, AgentThread};
    use reflect_tools::ToolRegistry;
    use std::path::Path;
    let cfg = AgentConfig::new("mock/mock-1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));
    let agent = Reflect::from_thread(thread);

    let mut stream = agent.submit(Submission::user_input("hi")).await;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        if let EventMsg::AgentMessageDelta(d) = event.msg {
            print!("{}", d.delta);
        }
    }
    println!();
    Ok(())
}
