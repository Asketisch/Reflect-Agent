//! Provider 配置 section（Anthropic / OpenAI / Ollama）及其共享类型。
//!
//! 包含 `CredentialConfig`、`QuotaConfig`、`QuotaSource` 和三个
//! provider section 定义。

use serde::{Deserialize, Serialize};

// ── 共享类型 ──────────────────────────────────────────────────

/// v1.0 多 Provider 路由:`[[<provider>.credentials]]` 数组里单条
/// 凭证的 schema 镜像。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CredentialConfig {
    /// 必填(用户诊断标识);缺省时 builder 兜底 `"env-N"` 或 `"default"`。
    pub label: String,
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// 该 plan 的模型名。解析优先级:env `REFLECT_MODEL` > 被钉住条目的
    /// `model` > `[<provider>].model` 段级覆盖 > 未配置(不编造内置默认)。
    /// GUI 的 coding plan 编辑器把 model 写在这里而非段级。
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 覆盖 `RoutingPolicy` 全局 cooldown 默认值,单位秒。
    #[serde(default)]
    pub cooldown_override_secs: Option<u64>,
    /// v1.x 功能 7:可选的 token plan 配额声明。
    #[serde(default)]
    pub quota: Option<QuotaConfig>,
}

fn default_weight() -> u32 {
    1
}

/// v1.x 功能 7:token plan 配额声明。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuotaConfig {
    /// 配额重置周期(秒)。如 5h = 18000。
    pub window_secs: u64,
    /// 窗口内最大 token 数(input + output)。
    pub max_tokens: u64,
    /// 配额数据来源。`None` = 本地统计(默认);`Some` = 调厂商 quota API。
    #[serde(default)]
    pub check_via: Option<QuotaSource>,
}

/// v1.x 功能 7:厂商 quota API 路由。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    Kimi,
    Zhipu,
    Minimax,
    Zenmux,
    Volcengine,
    AnthropicUsage,
    OpenAIUsage,
}

impl CredentialConfig {
    /// 转为 `reflect_llm::Credential`。
    pub fn to_credential(&self) -> reflect_llm::Credential {
        use std::time::Duration;
        reflect_llm::Credential {
            label: self.label.clone(),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            weight: if self.weight == 0 { 1 } else { self.weight },
            cooldown_override: self.cooldown_override_secs.map(Duration::from_secs),
        }
    }
}

// ── Provider Sections ──────────────────────────────────────────

/// Anthropic provider 配置 —— `[anthropic]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnthropicSection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

/// OpenAI provider 配置 —— `[openai]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OpenAISection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
    /// P2 `openai-responses`:`true` 时 builder 用 `OpenAIResponsesClient`
    /// (`/v1/responses`)替代 `OpenAIClient`(`/v1/chat/completions`)。
    #[serde(default)]
    pub responses_api: bool,
}

/// Ollama 本地 provider 配置 —— `[ollama]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OllamaSection {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub keep_alive_secs: Option<i64>,
    pub num_ctx: Option<u32>,
    pub num_gpu: Option<u32>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}
