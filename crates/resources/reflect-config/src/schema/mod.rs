//! 配置数据结构。所有字段 `Option<>` / `Default`，未知 TOML 键不报错。
//!
//! ## 子模块拆分
//!
//! - [`provider`] —— 各 provider 段(`AnthropicSection` / `OpenAISection` / `OllamaSection`) + `CredentialConfig`
//! - [`agent`] —— 路由 / 子代理 / 协调器 / 压缩 / token 预算
//! - [`notifications`] —— 通知 / 遥测 / 分析
//! - [`integrations`] —— MCP / LSP / PostgreSQL / Redis / Bridge / Voice / DAP / ACP / 网络搜索
//! - [`plugins`] —— 插件市场配置 / 插件段
//!
//! 所有子模块类型通过 `pub use` 重导出,外部代码仍走 `crate::schema::XxxSection`。

pub mod agent;
pub mod integrations;
pub mod notifications;
pub mod permission_syntax;
pub mod plugins;
pub mod provider;

pub use integrations::ACP_DEFAULT_BIND;

pub use agent::{
    AnthropicSubagentSection, CompactSection, CoordinatorSection, OllamaSubagentSection,
    OpenAISubagentSection, RoutingSection, SpecSlotConfig, SubagentProvidersSection,
    SubagentSpecConfig, TokenBudgetSection,
};
pub use integrations::{
    AcpSection, BridgeSection, DapSection, LspFilePattern, LspServerEntry, LspServersSection,
    McpServerEntry, McpServersSection, McpTransport, PostgresSessionSection, SseRedisSection,
    VoiceSection, WebSearchSection,
};
pub use notifications::{
    AnalyticsSection, NotificationChannel, NotificationsSection, TelemetrySection,
    TuiNotificationsSection,
};
pub use permission_syntax::parse_permission_entry;
pub use plugins::{PluginMarketplaceConfig, PluginMarketplaceKind, PluginsSection};
pub use provider::{
    AnthropicSection, CredentialConfig, OllamaSection, OpenAISection, QuotaConfig, QuotaSource,
};

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── 顶层配置 ──────────────────────────────────────────────────

/// 顶层配置 —— 对应 `~/.reflect/config.toml` 的根表。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReflectConfig {
    #[serde(default)]
    pub active: ActiveSection,
    #[serde(default)]
    pub anthropic: Option<AnthropicSection>,
    #[serde(default)]
    pub openai: Option<OpenAISection>,
    #[serde(default)]
    pub ollama: Option<OllamaSection>,
    #[serde(default)]
    pub compact: CompactSection,
    #[serde(default)]
    pub token_budget: Option<TokenBudgetSection>,
    #[serde(default)]
    pub sandbox: SandboxSection,
    #[serde(default)]
    pub hooks: HooksSection,
    #[serde(default)]
    pub mcp_servers: McpServersSection,
    #[serde(default)]
    pub lsp_servers: LspServersSection,
    #[serde(default)]
    pub plugins: PluginsSection,
    #[serde(default)]
    pub routing: Option<RoutingSection>,
    #[serde(default)]
    pub subagent_providers: Option<SubagentProvidersSection>,
    #[serde(default)]
    pub coordinator: Option<CoordinatorSection>,
    #[serde(default)]
    pub sanitize: Option<SanitizeSection>,
    #[serde(default)]
    pub ask_user_question: Option<AskUserQuestionSection>,
    #[serde(default)]
    pub model: Option<ModelSection>,
    /// 按模型覆盖上下文窗口(TOML:`[context_windows]`)。
    /// 优先于 `context_window_for` 静态回退表 —— 用来支持未被识别的
    /// 私端/新模型。
    /// 例子:
    /// ```toml
    /// [context_windows]        # 段名
    /// "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
    /// "qwen36-1m"  = 1000000   # 模型名 → 上下文 token 数
    /// ```
    #[serde(default)]
    pub context_windows: Option<ContextWindowsSection>,
    #[serde(default)]
    pub goal: Option<GoalSection>,
    #[serde(default)]
    pub telemetry: Option<TelemetrySection>,
    #[serde(default)]
    pub analytics: Option<AnalyticsSection>,
    #[serde(default)]
    pub notifications: Option<NotificationsSection>,
    #[serde(default)]
    pub postgres_session: Option<PostgresSessionSection>,
    #[serde(default)]
    pub sse_redis: Option<SseRedisSection>,
    #[serde(default)]
    pub bridge: Option<BridgeSection>,
    #[serde(default)]
    pub voice: Option<VoiceSection>,
    #[serde(default)]
    pub dap: Option<DapSection>,
    #[serde(default)]
    pub acp: Option<AcpSection>,
    #[serde(default)]
    pub web_search: Option<WebSearchSection>,
    /// 工具权限白名单 / 黑名单规则。加载时合并进 permission store(与
    /// `~/.reflect/permissions.toml` 并存,config 规则优先)。示例:
    /// ```toml
    /// [[permissions.rule]]        # 权限规则数组
    /// tool = "Bash"                 # 工具名
    /// action = "allow"              # 动作:allow / deny / ask
    /// shell_pattern = "git *"       # 仅 Bash,命令 glob(修复后生效)
    /// ```
    #[serde(default)]
    pub permissions: Option<PermissionsSection>,
    #[serde(default)]
    pub subagents: Vec<SubagentSpecConfig>,
    #[serde(default = "default_config_version")]
    pub config_version: u32,
}

pub const CURRENT_CONFIG_VERSION: u32 = 1;

fn default_config_version() -> u32 {
    1
}

impl Default for ReflectConfig {
    fn default() -> Self {
        Self {
            active: ActiveSection::default(),
            anthropic: None,
            openai: None,
            ollama: None,
            compact: CompactSection::default(),
            token_budget: None,
            sandbox: SandboxSection::default(),
            hooks: HooksSection::default(),
            mcp_servers: McpServersSection::default(),
            lsp_servers: LspServersSection::default(),
            plugins: PluginsSection::default(),
            routing: None,
            context_windows: None,
            subagent_providers: None,
            coordinator: None,
            sanitize: None,
            ask_user_question: None,
            model: None,
            goal: None,
            telemetry: None,
            analytics: None,
            notifications: None,
            postgres_session: None,
            sse_redis: None,
            bridge: None,
            voice: None,
            dap: None,
            acp: None,
            web_search: None,
            permissions: None,
            subagents: Vec::new(),
            config_version: CURRENT_CONFIG_VERSION,
        }
    }
}

// ── Active / 通用 Section ──────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActiveSection {
    pub provider: Option<String>,
    #[serde(default)]
    pub max_iterations: Option<u32>,
}

// ── 沙箱 / 提问 / 模型 / 特性开关 / 目标 ────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SandboxSection {
    #[serde(default)]
    pub os_level: bool,
    #[serde(default)]
    pub writable_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub allow_network: bool,
}

/// `[permissions]` 段 —— 工具权限规则,直接复用 `reflect_permissions::PermissionRule`
///(`~/.reflect/permissions.toml` 的同一数据结构)。加载时合并进 permission store,
/// config 规则优先于 file 规则(first-match-wins)。
///
/// 支持两种写法,可并存:
///
/// 1. 显式 `[[permissions.rule]]`(与 `permissions.toml` 一致,字段最全):
///    ```toml
///    [[permissions.rule]]       # 权限规则数组
///    tool = "Bash"               # 工具名
///    action = "allow"            # 动作:allow / deny / ask
///    shell_pattern = "git *"     # 命令 glob 模式
///    ```
///
/// 2. Claude Code 式紧凑数组([`allow`] / [`deny`]),适合批量放行/拒绝:
///    ```toml
///    [permissions]               # 权限段
///    allow = ["Bash", "Edit", "Write", "Read", "Bash(git:*)", "Bash(npm:*)"]   # 放行列表
///    deny  = ["Bash(curl:*)", "Bash(wget:*)"]                                  # 拒绝列表
///    ```
///    `Bash(git:*)` 会解析为 `shell_pattern = "git*"`(见
///    [`permission_syntax::parse_permission_entry`])。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionsSection {
    /// 权限规则列表。`rename = "rule"` 与 `permissions.toml` 的 `[[rule]]` 对齐。
    #[serde(default, rename = "rule")]
    pub rules: Vec<reflect_permissions::PermissionRule>,
    /// Claude Code 式放行列表。加载时经 [`Self::expanded_rules`] 展平为
    /// `PermissionRule`(action = `Allow`),与 `rules` 合并。
    #[serde(default)]
    pub allow: Vec<String>,
    /// Claude Code 式拒绝列表。加载时展平为 `PermissionRule`(action = `Deny`)。
    /// matcher 对 deny 做全局短路,故展开时置于 allow 之前以确保拒绝优先。
    #[serde(default)]
    pub deny: Vec<String>,
}

impl PermissionsSection {
    /// 把 `deny` + `allow` 紧凑字符串与显式 `rules` 合并成统一的规则列表。
    ///
    /// 展开顺序:**deny → allow → 显式 rules**。
    /// - deny 置于最前:matcher 的「deny 全局短路」(`evaluate_with_context`
    ///   见 `reflect-permissions/src/matcher.rs`)确保拒绝优先,不受后续 allow 覆盖。
    /// - 显式 `rules` 置于最后:它字段最全(含 `tool_glob` / `shell_pattern`),
    ///   放后面能被 allow/deny 的同工具规则先行短路,但 deny 已在前兜底,
    ///   实际表现为「deny 最强,其余 first-match-wins」。
    ///
    /// 无 `allow` / `deny` 时(向后兼容),返回值等价于原 `rules` 的克隆。
    pub fn expanded_rules(&self) -> Vec<reflect_permissions::PermissionRule> {
        use reflect_permissions::PermissionAction;
        let mut out = Vec::with_capacity(self.deny.len() + self.allow.len() + self.rules.len());
        for s in &self.deny {
            if let Some(r) = permission_syntax::parse_permission_entry(s, PermissionAction::Deny) {
                out.push(r);
            }
        }
        for s in &self.allow {
            if let Some(r) = permission_syntax::parse_permission_entry(s, PermissionAction::Allow) {
                out.push(r);
            }
        }
        out.extend(self.rules.iter().cloned());
        out
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AskUserQuestionSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_questions: Option<u8>,
    #[serde(default)]
    pub max_options: Option<u8>,
    #[serde(default)]
    pub default_timeout_secs: Option<u64>,
}

impl AskUserQuestionSection {
    pub fn resolved(&self) -> Option<ResolvedAskUserQuestion> {
        if !self.enabled.unwrap_or(true) {
            return None;
        }
        Some(ResolvedAskUserQuestion {
            max_questions: self.max_questions.unwrap_or(4).clamp(1, 4),
            max_options: self.max_options.unwrap_or(4).clamp(1, 4),
            default_timeout_secs: self.default_timeout_secs.unwrap_or(900),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAskUserQuestion {
    pub max_questions: u8,
    pub max_options: u8,
    pub default_timeout_secs: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelSection {
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub input_price_micro_usd_per_mtok: Option<u64>,
    #[serde(default)]
    pub output_price_micro_usd_per_mtok: Option<u64>,
}

impl ModelSection {
    pub fn input_price_usd_per_mtok(&self) -> Option<f64> {
        self.input_price_micro_usd_per_mtok
            .map(|v| v as f64 / 1_000_000.0)
    }
    pub fn output_price_usd_per_mtok(&self) -> Option<f64> {
        self.output_price_micro_usd_per_mtok
            .map(|v| v as f64 / 1_000_000.0)
    }
}

/// `[context_windows]` 段:per-model 上下文窗口覆盖表。
///
/// 优先级:该表 > 内置 `context_window_for` 静态回退表 > `None`。
/// key 不区分大小写(TOML 解析后 `ReflectConfig::context_window_overrides` 会 lowercase)。
///
/// 支持两种 TOML 写法:
/// 1. 扁平 key/value(推荐):
///    ```toml
///    [context_windows]        # 段名
///    "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
///    "qwen36-1m" = 1000000    # 模型名 → 上下文 token 数
///    ```
/// 2. 嵌套 `entries` 子段:
///    ```toml
///    [context_windows.entries]   # 嵌套子段
///    "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
///    ```
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ContextWindowsSection {
    pub entries: HashMap<String, u32>,
}

impl<'de> Deserialize<'de> for ContextWindowsSection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 先反序列化为 raw HashMap;允许多种形态,最后归并到 `entries`。
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Flat(HashMap<String, u32>),
            Nested {
                #[serde(default)]
                entries: HashMap<String, u32>,
            },
        }
        let raw = Either::deserialize(deserializer)?;
        let entries = match raw {
            Either::Flat(m) => m,
            Either::Nested { entries } => entries,
        };
        Ok(ContextWindowsSection { entries })
    }
}

impl ContextWindowsSection {
    /// 取归一化(去 provider/ollama-tag,小写化)后的窗口大小。
    /// key 完全匹配(归一化后优先)→ prefix 匹配 → 首个非空命中。
    pub fn lookup(&self, model_spec: &str) -> Option<u32> {
        let key = normalize_model_key(model_spec);
        if key.is_empty() {
            return None;
        }
        // 精确匹配(归一化后)。
        if let Some(v) = self.entries.get(&key) {
            return Some(*v);
        }
        // 前缀匹配(同 `context_window_for` 静态表的策略)。
        self.entries
            .iter()
            .find(|(k, _)| k.starts_with(&key) || key.starts_with(k.as_str()))
            .map(|(_, v)| *v)
    }
}

/// 模型名归一化(与 `reflect_llm::context_window_for` 对齐):
/// 去 provider 前缀(`anthropic/claude-...` → `claude-...`)、去 ollama tag(`:7b`),
/// 小写化 trim。
fn normalize_model_key(model_spec: &str) -> String {
    let no_provider = model_spec
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_spec);
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    no_tag.trim().to_ascii_lowercase()
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GoalSection {
    pub max_turns: Option<u32>,
    pub verification_model: Option<String>,
    pub verify_timeout_seconds: Option<u64>,
}

// ── 脱敏 / 钩子 ──────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SanitizeSection {
    pub enabled: Option<bool>,
    pub marker: Option<String>,
    pub disable_default_patterns: Option<bool>,
    pub extra_patterns: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HooksSection {
    pub enabled: Option<Vec<String>>,
    #[serde(default)]
    pub search_budget: Option<SearchBudgetSection>,
    #[serde(default)]
    pub test_runner: Option<TestRunnerSection>,
    #[serde(default)]
    pub plan_completion: Option<PlanCompletionSection>,
    #[serde(default)]
    pub verification: Option<VerificationSection>,
    #[serde(default)]
    pub langfuse_tracker: Option<LangfuseSection>,
    #[serde(default)]
    pub read_before_edit: Option<ReadBeforeEditSection>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SearchBudgetSection {
    pub max_calls: Option<u32>,
    pub search_tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TestRunnerSection {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlanCompletionSection {
    pub strict: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VerificationSection {
    pub run_on_stop: Option<bool>,
    pub test_command: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LangfuseSection {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub public_key: Option<String>,
    pub secret_key: Option<String>,
    pub export_mode: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReadBeforeEditSection {
    pub enabled: Option<bool>,
    pub mtime_drift_tolerance_ms: Option<u64>,
}

// ── 测试 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
