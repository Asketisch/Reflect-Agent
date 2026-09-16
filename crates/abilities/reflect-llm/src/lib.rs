#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! reflect-llm — ModelClient trait, Capabilities, provider 实现,以及 v1.0
//! 多 Provider 路由(`ModelRegistry` credential pool / `Credential` /
//! `RoutingPolicy` / `CooldownEntry`)。
//!
//! v0.3.1 内置三个 provider:OpenAI(Chat Completions)、Anthropic
//! (Messages)、Ollama(原生 `/api/chat` NDJSON 流)。
//! Responses API / MCP 推迟至 v1+ 引入。
//!
//! v1.0: 新增 `Credential` / `CredentialPool` / `RoutingPolicy` /
//! `CooldownEntry` 支持 round-robin credentials + cooldown + per-role
//! 路由。`ModelRegistry` 从单值 `HashMap<name, client>` 升级为
//! `HashMap<name, CredentialPool>` 池化结构,旧 `register` / `get` /
//! `resolve` / `list` API 保留(部分 `#[deprecated]`)。

pub mod capabilities;
pub mod client;
pub mod cooldown;
pub mod credential;
pub mod error;
pub mod event;
pub mod policy;
pub mod providers;
pub mod quota;
pub mod registry;
pub mod request;

pub use capabilities::{Capabilities, ProviderKind};
pub use client::ModelClient;
pub use cooldown::{CooldownEntry, CooldownReason};
pub use credential::Credential;
pub use error::LlmError;
pub use event::ChatEvent;
pub use policy::{Role, RoutingPolicy, SpecSlot};
pub use providers::{
    AnthropicClient, AnthropicConfig, DEFAULT_MOCK_MODEL, DEFAULT_MOCK_REPLY, ENV_MOCK_SCRIPT,
    KimiQuotaProvider, MinimaxQuotaProvider, MockClient, MockReply, ModelPricing, OllamaClient,
    OllamaConfig, OpenAIClient, OpenAIConfig, OpenAIResponsesClient, OpenAIResponsesConfig,
    QuotaError, QuotaProvider, QuotaSnapshot, ZenmuxQuotaProvider, ZhipuQuotaProvider,
    context_window_for, cumulative_cost_usd, is_priced, price,
};
pub use quota::{CredentialKey, QuotaConfig, QuotaSource, QuotaTracker, SharedQuotaTracker};
pub use registry::{CredentialPool, ModelRegistry, NextClient, PoolEntry, SharedModelRegistry};
pub use request::{
    AssistantContent, CacheBreak, CacheControl, CacheControlKind, CacheTtl, ChatMessage,
    ChatRequest, ContentBlock, ReasoningEffort, ResponseFormat, SystemBlock, SystemBlocks,
    ThinkingConfig, ToolCallRequest, ToolResult, ToolSpec, UserContent,
};
