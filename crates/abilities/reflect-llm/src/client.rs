//! `ModelClient` trait —— 所有 provider 实现的抽象接口。

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::capabilities::{Capabilities, ProviderKind};
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::model_catalog::ModelInfo;
use crate::request::ChatRequest;

pub type BoxedModelClient = std::sync::Arc<dyn ModelClient>;

/// v1.4 B2:`complete()` 的一次性产出。`text` 是全部 `ContentDelta`
/// 的拼接;`usage` 是流中的用量快照(`ChatEvent::Usage`,provider 不报
/// 则为 `None`)。
#[derive(Debug, Clone, Default)]
pub struct CompleteOutput {
    pub text: String,
    pub usage: Option<crate::event::UsageSnapshot>,
}

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

    /// v1.4 B2:非流式便捷接口 —— 内部走 [`Self::stream`] 收集到流结束,
    /// 拼接全部文本增量并捕获最后一次用量快照。默认实现让所有 provider
    /// (含 plugin / 测试 stub)免实现即获得;provider 有原生非流式
    /// endpoint 时可 override 优化。结构化输出调用方(goal 校验 / 讨论
    /// 裁判)用它 + `ChatRequest::response_format` 一次拿完整 JSON。
    ///
    /// 流中途的 `ChatEvent::Error` 视为致命(与「收集到底」的调用方语义
    /// 一致 —— 非流式调用方无法对中途错误做续接),直接返回 `Err`。
    async fn complete(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<CompleteOutput, LlmError> {
        use futures::StreamExt;
        let mut stream = self.stream(request, cancel).await?;
        let mut out = CompleteOutput::default();
        while let Some(item) = stream.next().await {
            match item? {
                ChatEvent::ContentDelta(d) => out.text.push_str(&d),
                ChatEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens,
                } => {
                    out.usage = Some(crate::event::UsageSnapshot {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        cache_write_tokens,
                    });
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// 拉取 provider 当前可用的**模型清单**。
    ///
    /// 默认实现返回 `Ok(None)` —— 该 provider 没有清单端点(诚实降级,
    /// **不编造**)。实现了清单端点的 provider(OpenAI 兼容 / Ollama)
    /// override 本方法。
    ///
    /// 语义:
    /// - `Ok(None)` = 该 provider 不提供清单;
    /// - `Ok(Some(list))` = 清单拿到了(可能为空列表);
    /// - `Err` = 端点存在但调用失败(网络 / 鉴权 / 解析)。
    ///
    /// 清单条目里的「免费」判定由 [`crate::model_catalog::FreeModelRules`]
    /// 负责 —— provider 披露的字段在不同方言下差别很大,判定规则独立于
    /// 清单本身。
    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Option<Vec<ModelInfo>>, LlmError> {
        Ok(None)
    }
}
