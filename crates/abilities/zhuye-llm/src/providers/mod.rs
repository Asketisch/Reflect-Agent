//! Provider implementations.

pub mod anthropic;
pub mod context_window;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod pricing;
pub mod quota;
pub mod responses;

use crate::error::LlmError;
use crate::request::ChatRequest;

// ── provider 共享的端点拼接与 SSE 响应校验 ─────────────────────────

/// 把 base_url 与 API path 拼成完整端点。
///
/// base 的最后一段已是版本段(形如 `v1` / `v4`,仅含数字后缀)时
/// **不再追加 `/v1`**,直接接 path;否则补 `/v1`。三种实测形态:
/// - `https://api.openai.com` → `…/v1/chat/completions`(原行为);
/// - `https://gw.example.com/v1` → `…/v1/chat/completions`(修复:原来
///   拼成 `/v1/v1/…` 直接 404);
/// - `https://open.bigmodel.cn/api/paas/v4` → `…/v4/chat/completions`
///   (修复:版本段不是 v1 的网关原来无法表达)。
pub(crate) fn join_versioned_endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let last = base.rsplit('/').next().unwrap_or("");
    let is_version_segment =
        last.len() >= 2 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit());
    if is_version_segment {
        format!("{base}{path}")
    } else {
        format!("{base}/v1{path}")
    }
}

/// 校验「流式请求」的成功响应确实是 `text/event-stream`。
///
/// 引擎以 `stream=true` 发请求,但部分网关对不支持流式的模型会返回
/// 普通 JSON(HTTP 200)。此前该响应交给 SSE 解析器,一条事件也解析
/// 不出来 → 流以「成功但零内容」收场,`prompt()` 返回空串、无任何
/// 错误信号 —— 协议层故障被伪装成「模型没说话」。现在显式返回
/// `LlmError::Provider`(GiveUp 路径,不无谓重试),错误消息带
/// content-type 与响应体预览,引擎发 `EventMsg::Error`、SDK 升为异常。
pub(crate) async fn ensure_sse_response(
    response: reqwest::Response,
    endpoint: &str,
) -> Result<reqwest::Response, LlmError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let preview: String = body.chars().take(512).collect();
    Err(LlmError::Provider {
        status,
        message: format!(
            "流式请求收到非 SSE 响应(content-type: '{}');端点 {endpoint} 可能不支持 \
             stream=true,或把错误载荷当 200 返回了。响应体预览: {preview}",
            if content_type.is_empty() {
                "缺失"
            } else {
                &content_type
            }
        ),
    })
}

/// 把 provider 段配置的 temperature / max_tokens 作为**默认值**
/// 合入请求(请求侧显式值优先)。`ChatRequest` 是各 provider 共用的
/// 入参形态,在 `stream()` 入口统一做一次默认值回填,比在各
/// `XxxRequest::from` 里分散处理更不易漏。
pub(crate) fn apply_request_param_defaults(
    request: &mut ChatRequest,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) {
    if request.temperature.is_none() {
        request.temperature = temperature;
    }
    if request.max_tokens.is_none() {
        request.max_tokens = max_tokens;
    }
}

pub use self::anthropic::{AnthropicClient, AnthropicConfig};
pub use self::context_window::context_window_for;
pub use self::mock::{
    DEFAULT_MOCK_MODEL, DEFAULT_MOCK_REPLY, ENV_MOCK_SCRIPT, MockClient, MockReply,
};
pub use self::ollama::{OllamaClient, OllamaConfig};
pub use self::openai::{OpenAIClient, OpenAIConfig};
pub use self::pricing::{ModelPricing, cumulative_cost_usd, is_priced, price};
pub use self::quota::{
    KimiQuotaProvider, MinimaxQuotaProvider, QuotaError, QuotaProvider, QuotaSnapshot,
    ZenmuxQuotaProvider, ZhipuQuotaProvider,
};
pub use self::responses::{OpenAIResponsesClient, OpenAIResponsesConfig};
