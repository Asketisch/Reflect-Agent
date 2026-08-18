//! `reflect-config` — 统一 TOML 配置加载 + 热重载。
//!
//! M7 起取代 `reflect-hooks::config::HooksConfig` 的单一职责，
//! 把 provider / compact / hooks 配置集中到 `~/.reflect/config.toml`。
//!
//! ## 模块
//! - [`schema`] —— 数据结构 (`ReflectConfig` 及各 section)
//! - [`load`] —— 单次同步加载
//! - [`watch`] —— 基于 `notify` 的热重载 (debounce 250ms)
//! - [`builder`] —— `ReflectConfig → ModelRegistry`
//!
//! ## 配置示例
//! ```toml
//! [active]
//! provider = "anthropic"   # 主 provider 名,值取自下方 provider 段名
//!
//! [anthropic]              # Anthropic provider 段
//! api_key = "sk-ant-..."   # Anthropic API key(也可由 ANTHROPIC_API_KEY env 提供)
//! base_url = "https://api.anthropic.com"   # 可选:自定义 endpoint
//! model = "claude-3-5-sonnet-latest"       # 可选:默认模型
//!
//! [openai]                 # OpenAI provider 段
//! api_key = "sk-..."       # OpenAI API key(也可由 OPENAI_API_KEY env 提供)
//!
//! [compact]                # 压缩策略段
//! trigger_tokens = 10000   # 触发自动压缩的输入 token 阈值
//!
//! [hooks]                  # 钩子总段
//! enabled = ["search_budget", "verification"]   # 启用的内置钩子列表
//!
//! [hooks.search_budget]    # search_budget 钩子专属配置
//! max_calls = 20           # 单 turn 最大搜索调用次数
//! ```

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

pub mod analytics;
pub mod builder;
pub mod error;
pub mod load;
pub mod schema;
pub mod watch;

pub use analytics::{
    ExporterGuard, Health, default_service_name, init_exporter, is_enabled as analytics_enabled,
    status_line as analytics_status_line,
};
pub use builder::{LspServerConfigShape, McpServerConfigShape};
pub use error::ConfigError;
pub use load::{default_config_path, load_default, load_from_file, load_from_str};
pub use schema::{
    ACP_DEFAULT_BIND, AcpSection, ActiveSection, AnalyticsSection, AnthropicSection,
    AskUserQuestionSection, BridgeSection, CompactSection, ContextWindowsSection,
    CoordinatorSection, CredentialConfig, DapSection, GoalSection, HooksSection, LangfuseSection,
    LspFilePattern, LspServerEntry, LspServersSection, McpServerEntry, McpServersSection,
    McpTransport, ModelSection, NotificationChannel, NotificationsSection, OllamaSection,
    OpenAISection, PermissionsSection, PlanCompletionSection, PostgresSessionSection, QuotaConfig,
    QuotaSource, ReadBeforeEditSection, ReflectConfig, ResolvedAskUserQuestion, SandboxSection,
    SanitizeSection, SearchBudgetSection, SseRedisSection, SubagentSpecConfig, TelemetrySection,
    TestRunnerSection, TokenBudgetSection, TuiNotificationsSection, VerificationSection,
    VoiceSection, WebSearchSection, parse_permission_entry,
};
pub use watch::{ConfigWatcher, WatcherHandle};

/// 配置目录的默认相对名（位于 `$HOME` 下）。
pub const DEFAULT_CONFIG_DIR: &str = ".reflect";
/// 默认配置文件名。
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";
