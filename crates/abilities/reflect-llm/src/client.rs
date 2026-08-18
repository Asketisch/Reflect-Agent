//! `ModelClient` trait —— 所有 provider 实现的抽象接口。

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::capabilities::{Capabilities, ProviderKind};
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::request::ChatRequest;

pub type BoxedModelClient = std::sync::Arc<dyn ModelClient>;

/// 以流式为优先的 LLM client。实现把 `ChatRequest` 映射成 provider 特定的
/// HTTP 调用,解析 SSE 响应,并产出 `ChatEvent`。
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Provider 名称(如 `"openai"`)。
    fn name(&self) -> &str;

    /// 对外声明的静态能力。
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// v1.0.0-rc2: 强类型 provider 标识。默认 `Custom` —— 让 plugin /
    /// test stub 无需 override 即可编译。3 个内置 client 各自 override
    /// 为 `Anthropic` / `OpenAI` / `Ollama`。
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Custom
    }

    /// 流式执行一次 chat completion。返回 `Result` 是为了承载同步失败
    /// (auth、model not found 等)。流式错误以 `ChatEvent::Error` 形式
    /// 上报,可能终止也可能不终止整条流。
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>;
}
