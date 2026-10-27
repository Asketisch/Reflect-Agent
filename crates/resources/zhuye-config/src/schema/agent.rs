//! Agent / routing / subagent / coordinator / compact / token budget 配置。
//!
//! 包含:
//! - `RoutingSection` / `SpecSlotConfig`
//! - `SubagentProvidersSection` / `SubagentSpecConfig`
//! - `CoordinatorSection` / `CompactSection` / `TokenBudgetSection`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::schema::provider::CredentialConfig;

// ── v1.0 多 Provider 路由:per-role 路由策略段 ─────────────────

/// v1.0 多 Provider 路由:`[routing]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RoutingSection {
    #[serde(default)]
    pub main: SpecSlotConfig,
    #[serde(default)]
    pub compact: SpecSlotConfig,
    #[serde(default)]
    pub subagent: SpecSlotConfig,
    /// 单轮 LLM 调用的最大尝试次数(含同凭证重试 / 冷却 / failover)。
    /// 缺省用 `RoutingPolicy::default()` 的 16;调小(如 10)可限制最坏
    /// 情况下的空转时长。
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

/// 单角色 slot 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpecSlotConfig {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default)]
    pub weights: Vec<u32>,
}

// ── 子代理 provider 配置 ──────────────────────────────────────

/// `[subagent_providers]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentProvidersSection {
    #[serde(default)]
    pub anthropic: Option<AnthropicSubagentSection>,
    #[serde(default)]
    pub openai: Option<OpenAISubagentSection>,
    #[serde(default)]
    pub ollama: Option<OllamaSubagentSection>,
}

/// Subagent 专用的 Anthropic provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnthropicSubagentSection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

/// Subagent 专用的 OpenAI provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OpenAISubagentSection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

/// Subagent 专用的 Ollama provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OllamaSubagentSection {
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

// ── v1.x 功能 2:用户自定义 subagent spec ──────────────────────

/// v1.x 功能 2:`[[subagents]]` 数组里单条 subagent spec。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentSpecConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

// ── 协调器配置 ──────────────────────────────────────────────────

/// v1.1.0 Phase 4:`[coordinator]` 段配置定义。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CoordinatorSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub system_prompt_path: Option<PathBuf>,
    #[serde(default)]
    pub max_workers: Option<u8>,
}

// ── 压缩与预算 ──────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CompactSection {
    pub trigger_tokens: Option<u32>,
}

/// v1.2 P1-12:Token 预算上限(`[token_budget]` 段)。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenBudgetSection {
    #[serde(default)]
    pub session_total_tokens: Option<u64>,
    #[serde(default)]
    pub per_turn_input_tokens: Option<u32>,
}
