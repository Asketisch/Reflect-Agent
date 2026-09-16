//! `ReflectConfig → ModelRegistry` 转换。
//!
//! - 若 `[anthropic].api_key` 缺,回退到 env `ANTHROPIC_API_KEY`。
//! - 若 `[openai].api_key` 缺,回退到 env `OPENAI_API_KEY`。
//! - `active_provider()` 优先级:`REFLECT_PROVIDER` env > TOML `[active].provider` > 第一个非空 section。
//! - `active_credential()`:`[active].credential` 钉住 active provider
//!   凭证池中的条目;`apply_to_registry` 把它注册为 registry 的
//!   preferred label —— 该条目健康则始终优先派位,其余条目仅在其
//!   cooldown 时作 failover。
//!
//! v1.0 多 Provider 路由:
//! - `[[<provider>.credentials]]` 数组非空 → 展开为 `CredentialPool`;
//! - 数组空 + 单值 `api_key` 非空(env 兜底也算)→ wrap 为
//!   `label = "default"` 的单 entry pool,行为与 v0.x 完全一致;
//! - 都空 → 该 provider 不注册。
//!
//! model 解析(`resolve_model`)是**诚实**的:env `REFLECT_MODEL` >
//! 被钉住条目的 `model` > `[<provider>].model` > `None`。未显式配置时
//! 返回 `None` 而非编造内置默认 —— 请求打向第三方兼容端点(stepfun /
//! MiniMax 等)时,编造的官方模型名只会产生更难诊断的远端错误,不如让
//! 缺失在配置层就可见。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reflect_llm::{
    AnthropicClient, AnthropicConfig, CredentialPool, MockClient, ModelClient, ModelRegistry,
    OllamaClient, OllamaConfig, OpenAIClient, OpenAIConfig, OpenAIResponsesClient,
    OpenAIResponsesConfig, PoolEntry,
};

use crate::error::ConfigError;
use crate::schema::{
    AnthropicSection, AnthropicSubagentSection, CredentialConfig, LspFilePattern, LspServerEntry,
    McpServerEntry, McpTransport, OllamaSection, OllamaSubagentSection, OpenAISection,
    OpenAISubagentSection, ReflectConfig, SpecSlotConfig,
};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// MCP tool call 默认超时 30s,与 `mcp.connection.timeoutMs` (Anthropic) 一致。
const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;
/// LSP request 默认超时 30s,与 MCP 对齐。
const DEFAULT_LSP_TIMEOUT_MS: u64 = 30_000;
const ENV_REFLECT_PROVIDER: &str = "REFLECT_PROVIDER";
const ENV_REFLECT_MODEL: &str = "REFLECT_MODEL";
const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
/// Ollama 启动时可被 `apply_to_registry` 识别的 env。本地 `ollama serve`
/// 一般不需要 key,但 Ollama Cloud / 反向代理可能设 `OLLAMA_HOST` 指
/// 向远端 server,或 `OLLAMA_API_KEY` 带 Bearer。
const ENV_OLLAMA_HOST: &str = "OLLAMA_HOST";
const ENV_OLLAMA_API_KEY: &str = "OLLAMA_API_KEY";

impl ReflectConfig {
    /// 构造一个新的 `ModelRegistry`,把本配置中所有非空 provider 注册进去。
    pub fn to_registry(&self) -> Result<ModelRegistry, ConfigError> {
        let r = ModelRegistry::new();
        self.apply_to_registry(&r)?;
        Ok(r)
    }

    /// 把本配置的 provider 注册到已有 `ModelRegistry`(用于热重载)。
    /// 注意:`apply_to_registry` 不清空已有注册 —— 调用方负责决定何时 unregister。
    ///
    /// v1.0 多 Provider 路由:每个 provider 段走 `register_pool`,旧
    /// `api_key = "..."` 单值在 builder 内部 wrap 为
    /// `label = "default"` / `weight = 1` 的单 entry pool。
    pub fn apply_to_registry(&self, registry: &ModelRegistry) -> Result<(), ConfigError> {
        if let Some(anth) = &self.anthropic {
            if let Some(pool) = build_anthropic_pool(anth)? {
                registry.register_pool("anthropic", pool);
            }
        }
        if let Some(oai) = &self.openai {
            if let Some(pool) = build_openai_pool(oai)? {
                registry.register_pool("openai", pool);
            }
        }
        // v0.3.1: Ollama 注册 —— 只要 `[ollama]` 段存在或 env `OLLAMA_HOST`
        // 显式设了就注册(api_key 可选,本地 `ollama serve` 不需要认证)。
        if let Some(pool) = build_ollama_pool(self.ollama.as_ref())? {
            registry.register_pool("ollama", pool);
        }
        // v1.3 SDK:mock provider —— `REFLECT_PROVIDER=mock` 或
        // `REFLECT_MODEL` 形如 `mock` / `mock/...` 时注册(离线测试用,
        // 免 key、零网络)。显式 opt-in,不作为无 key 时的静默兜底。
        if matches!(self.active_provider(), Some("mock")) {
            registry.register_pool(
                "mock",
                CredentialPool {
                    entries: vec![PoolEntry {
                        client: Arc::new(MockClient::from_env()),
                        label: "default".into(),
                        weight: 1,
                    }],
                },
            );
        }
        // v1.5:[active].credential 钉住 —— active provider 池内同名条目
        // 健康则始终优先派位,其余条目只在其 cooldown 时作 failover。
        // label 未命中池内条目时 registry 侧静默忽略(全池 round-robin),
        // 不让一个手写错的 label 阻断启动。
        if let (Some(provider), Some(label)) = (self.active_provider(), self.active_credential()) {
            registry.set_preferred(provider, &label);
        }
        Ok(())
    }

    /// 选定的 provider 名;返回 `None` 表示未配置任何可用 provider。
    pub fn active_provider(&self) -> Option<&'static str> {
        self.active_provider_inner(
            std::env::var(ENV_REFLECT_PROVIDER).ok().as_deref(),
            std::env::var(ENV_REFLECT_MODEL).ok().as_deref(),
        )
    }

    /// `active_provider` 的纯函数核心(env 值由调用方注入,便于单测并行
    /// 运行而不必改写进程 env)。
    fn active_provider_inner(
        &self,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Option<&'static str> {
        if let Some(p) = env_provider {
            if let Some(name) = canonical_provider(p) {
                return Some(name);
            }
        }
        // v1.3 SDK:`REFLECT_MODEL` 以完整 spec 形式(`mock/mock-1`)或裸
        // `mock` 显式指定 mock 时优先选中 —— 优先级在 TOML `[active]`
        // 之前,让"用户明确要 mock"总能赢过配置文件里的真实 provider。
        if let Some(m) = env_model.map(str::trim) {
            if m == "mock" || m.starts_with("mock/") {
                return Some("mock");
            }
        }
        if let Some(p) = &self.active.provider {
            if let Some(name) = canonical_provider(p) {
                return Some(name);
            }
        }
        // v1.0: 选 active 时看 `credentials` 数组是否非空,或单值 `api_key`
        // / env 是否存在 —— 与 v0.x 行为一致。
        if self
            .anthropic
            .as_ref()
            .is_some_and(|s| has_anthropic_credential(s))
        {
            return Some("anthropic");
        }
        if self
            .openai
            .as_ref()
            .is_some_and(|s| has_openai_credential(s))
        {
            return Some("openai");
        }
        // v0.3.1: Ollama fall-back —— 仅当用户显式声明(段存在 / env 设了)。
        if self.ollama.is_some() || env_has(ENV_OLLAMA_HOST) || env_has(ENV_OLLAMA_API_KEY) {
            return Some("ollama");
        }
        None
    }

    /// `[active].credential` —— 钉住 active provider 凭证池中的条目 label。
    /// `None` = 未钉住(全池加权 round-robin)。顶层 `[<provider>].api_key`
    /// 隐式条目的 label 固定为 `"default"`,钉它就显式写
    /// `credential = "default"`。
    pub fn active_credential(&self) -> Option<String> {
        self.active
            .credential
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// 给定 provider 显式解析 model:env `REFLECT_MODEL` > 被钉住条目的
    /// `model` > `[<provider>].model` 段级覆盖。
    ///
    /// `None` = 未显式配置任何 model。调用方应如实上报缺失(报错 /
    /// UI 显示"未配置"),而非回落内置默认 —— 默认模型名打向第三方
    /// 兼容端点只会得到更难诊断的远端错误。
    pub fn resolve_model(&self, provider: &str) -> Option<String> {
        self.resolve_model_inner(provider, std::env::var(ENV_REFLECT_MODEL).ok().as_deref())
    }

    /// `resolve_model` 的纯函数核心(env 值由调用方注入,便于单测)。
    fn resolve_model_inner(&self, provider: &str, env_model: Option<&str>) -> Option<String> {
        // v1.3 SDK:mock provider 的 model 取 `mock/` 前缀后的部分
        // (`REFLECT_MODEL=mock/mock-1` → `mock-1`),裸 `mock` / 未设 →
        // `mock-1`。不能走下面的通用 env 分支,否则会把完整 spec
        // (`mock/mock-1`)原样当 model 名拼出 `mock/mock/mock-1`。
        // mock 是显式 opt-in 的离线 provider,内置 model 名属于其契约
        // 的一部分,不属于"编造默认"。
        if provider == "mock" {
            if let Some(m) = env_model.map(str::trim) {
                if let Some(stripped) = m.strip_prefix("mock/") {
                    if !stripped.is_empty() {
                        return Some(stripped.to_string());
                    }
                }
            }
            return Some(reflect_llm::DEFAULT_MOCK_MODEL.to_string());
        }
        if let Some(m) = env_model.map(str::trim).filter(|m| !m.is_empty()) {
            return Some(m.to_string());
        }
        // 被钉住的凭证条目自带 model 时优先取用:它比段级覆盖更具体
        // (描述的就是当前 plan 本身);钉住只对 active provider 生效。
        if self.active_provider() == Some(provider) {
            if let Some(label) = self.active_credential() {
                let entry_model = self
                    .credentials_of(provider)
                    .and_then(|creds| creds.iter().find(|c| c.label == label))
                    .and_then(|c| c.model.clone());
                if let Some(m) = entry_model.filter(|m| !m.trim().is_empty()) {
                    return Some(m);
                }
            }
        }
        let section_override = self.section_model_of(provider);
        if let Some(m) = section_override.filter(|m| !m.trim().is_empty()) {
            return Some(m);
        }
        None
    }

    /// provider 段的 `credentials` 数组(mock 无段,返回 `None`)。
    fn credentials_of(&self, provider: &str) -> Option<&Vec<CredentialConfig>> {
        match provider {
            "anthropic" => self.anthropic.as_ref().map(|s| &s.credentials),
            "openai" => self.openai.as_ref().map(|s| &s.credentials),
            "ollama" => self.ollama.as_ref().map(|s| &s.credentials),
            _ => None,
        }
    }

    /// provider 段的段级 `model` 覆盖。
    fn section_model_of(&self, provider: &str) -> Option<String> {
        match provider {
            "anthropic" => self.anthropic.as_ref().and_then(|s| s.model.clone()),
            "openai" => self.openai.as_ref().and_then(|s| s.model.clone()),
            "ollama" => self.ollama.as_ref().and_then(|s| s.model.clone()),
            _ => None,
        }
    }

    /// 当前 active provider 的完整 spec(`"anthropic/<model>"`)。
    /// `None` = 未配置 provider,或 provider 已配置但没有任何显式 model
    /// (env / 被钉住条目 / 段级都没有)—— 调用方据此如实显示"未配置
    /// 模型",而不是展示一个编造出来的默认模型。
    ///
    /// 用途:`reflect-exec::handle_reload` 拿到 `old_cfg` 与 `new_cfg` 后,
    /// 对比两侧 `resolved_model_spec()` 的差异决定是否更新
    /// `AgentConfig.model`。集中在这里便于单测覆盖 env > 钉住条目 >
    /// 段级优先级和 provider 切换场景。
    pub fn resolved_model_spec(&self) -> Option<String> {
        let provider = self.active_provider()?;
        let model = self.resolve_model(provider)?;
        Some(format!("{provider}/{model}"))
    }

    /// v1.0 多 Provider 路由:从 `[routing]` 段构建 `RoutingPolicy`。
    ///
    /// 缺省规则:整个 `[routing]` 段缺省 → `RoutingPolicy::default()`(所有
    /// slot 的 primary 为空,`reflect-exec::bootstrap_m4` 用
    /// `cfg.resolved_model_spec()` 兜底 main slot)。
    /// 各 slot 的 `primary` 缺省时,优先用该 slot 的 env 变量:
    /// `REFLECT_COMPACT_MODEL` / `REFLECT_SUBAGENT_MODEL`,否则兜底
    /// active spec。
    pub fn routing_policy(&self) -> reflect_llm::RoutingPolicy {
        use reflect_llm::{Role, RoutingPolicy, SpecSlot};
        let active_spec = self.resolved_model_spec().unwrap_or_default();

        let Some(section) = &self.routing else {
            return RoutingPolicy {
                main: SpecSlot::with_primary(active_spec),
                ..RoutingPolicy::default()
            };
        };

        fn build_slot(slot: &SpecSlotConfig, env_var: &str, active_spec: &str) -> SpecSlot {
            let primary = slot
                .primary
                .clone()
                .or_else(|| std::env::var(env_var).ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| active_spec.to_string());
            // weights 与 fallbacks 长度不匹配时用全 1
            let weights = if slot.weights.len() == slot.fallbacks.len() {
                slot.weights.clone()
            } else {
                vec![1; slot.fallbacks.len()]
            };
            SpecSlot {
                primary,
                fallbacks: slot.fallbacks.clone(),
                weights,
            }
        }

        // v1.5 review:单轮重试上限可配 —— 缺省沿用 `RoutingPolicy::default()`
        // 的 16,`[routing] max_attempts` 覆盖。经 `with_policy` 流入
        // `model_call` 的重试环。
        let max_attempts = section
            .max_attempts
            .unwrap_or(RoutingPolicy::default().max_attempts);
        RoutingPolicy {
            main: build_slot(&section.main, "REFLECT_MAIN_MODEL", &active_spec),
            compact: build_slot(&section.compact, "REFLECT_COMPACT_MODEL", &active_spec),
            subagent: build_slot(&section.subagent, "REFLECT_SUBAGENT_MODEL", &active_spec),
            max_attempts,
            ..RoutingPolicy::default()
        }
        // 抑制 Role 导入的 unused 警告(供 Phase 3 caller 用)
        .with_role(Role::Main)
    }
}

// v1.0 多 Provider 路由:`RoutingPolicy::with_role` 内部辅助,避免
// 在 `routing_policy()` 末尾孤悬一个 Role import。
trait RoutingPolicyExt {
    fn with_role(self, _r: reflect_llm::Role) -> Self;
}
impl RoutingPolicyExt for reflect_llm::RoutingPolicy {
    fn with_role(self, _r: reflect_llm::Role) -> Self {
        self
    }
}

fn resolve_anthropic_key(s: &AnthropicSection) -> String {
    if let Some(key) = &s.api_key {
        if !key.is_empty() {
            return key.clone();
        }
    }
    std::env::var(ENV_ANTHROPIC_API_KEY).unwrap_or_default()
}

fn resolve_openai_key(s: &OpenAISection) -> String {
    if let Some(key) = &s.api_key {
        if !key.is_empty() {
            return key.clone();
        }
    }
    std::env::var(ENV_OPENAI_API_KEY).unwrap_or_default()
}

/// 把 `[anthropic]` 段展开为 `CredentialPool`。
///
/// 决策:
/// 1. `credentials` 数组非空 → 直接展开,各 entry 独立 `AnthropicClient`;
/// 2. 数组空 + 单值 `api_key` 非空 / `ANTHROPIC_API_KEY` env 存在 →
///    wrap 为 `label = "default"` / `weight = 1` 的单 entry pool(行为
///    与 v0.x 一致);
/// 3. 都空 → 返回 `None`,该 provider 不注册。
///
/// 出错:任一 `AnthropicClient::new` 失败 → `ConfigError::Build`。
fn build_anthropic_pool(section: &AnthropicSection) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = resolve_anthropic_key(section);
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = AnthropicConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            timeout: default_timeout,
        };
        let client =
            AnthropicClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client: Arc::new(client),
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// 把 `[openai]` 段展开为 `CredentialPool`,语义与 `build_anthropic_pool`
/// 镜像。
fn build_openai_pool(section: &OpenAISection) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = resolve_openai_key(section);
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let default_model = section.model.clone().unwrap_or_default();
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let base = cfg.base_url.clone().or_else(|| section.base_url.clone());
        // P2 `openai-responses`:`responses_api = true` 时用 Responses client,
        // 否则 Chat Completions(默认,向后兼容)。
        let client: Arc<dyn ModelClient> = if section.responses_api {
            let client_cfg = OpenAIResponsesConfig {
                api_key: cfg.api_key.clone(),
                base_url: base,
                timeout_secs: default_timeout.as_secs(),
                model: default_model.clone(),
            };
            Arc::new(
                OpenAIResponsesClient::new(client_cfg)
                    .map_err(|e| ConfigError::Build(e.to_string()))?,
            )
        } else {
            let client_cfg = OpenAIConfig {
                api_key: cfg.api_key.clone(),
                base_url: base,
                timeout: default_timeout,
            };
            Arc::new(OpenAIClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?)
        };
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client,
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// 把 `[ollama]` 段 + env 展开为 `CredentialPool`。
///
/// **注册条件**:用户必须显式声明 —— 段存在 OR `OLLAMA_HOST` / `OLLAMA_API_KEY`
/// env 任一被设。`api_key` 段内 / env 任一即可(本地 `ollama serve` 不需要
/// 任何一个,此函数返回 `Some(pool)` 允许无 key 注册)。
fn build_ollama_pool(
    section: Option<&OllamaSection>,
) -> Result<Option<CredentialPool>, ConfigError> {
    let section_present = section.is_some();
    let env_present = env_has(ENV_OLLAMA_HOST) || env_has(ENV_OLLAMA_API_KEY);
    if !section_present && !env_present {
        return Ok(None);
    }
    let section = section.cloned().unwrap_or_default();
    let env_base_url = std::env::var(ENV_OLLAMA_HOST).ok();
    let env_api_key = std::env::var(ENV_OLLAMA_API_KEY).ok();
    let merged_base_url = section.base_url.clone().or(env_base_url);
    let merged_api_key = section.api_key.clone().or(env_api_key); // OllamaSection.api_key 仍是 Option<String>
    let merged_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(120));
    let merged_model = section.model.clone();

    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        // 本地 ollama 无 key 也允许注册(label "default" / 拿 env 兜底 key)。
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: merged_api_key.clone().unwrap_or_default(),
            base_url: merged_base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = OllamaConfig {
            base_url: cfg.base_url.clone().or(merged_base_url.clone()),
            api_key: if cfg.api_key.is_empty() {
                merged_api_key.clone()
            } else {
                Some(cfg.api_key.clone())
            },
            keep_alive_secs: section.keep_alive_secs,
            num_ctx: section.num_ctx,
            num_gpu: section.num_gpu,
            timeout: merged_timeout,
            model: merged_model.clone(),
        };
        let client =
            OllamaClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client: Arc::new(client),
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// v1.0 多 Provider 路由:`active_provider` 在 anthropic / openai 段
/// 是否有可用 credential —— 检查 `credentials` 数组或单值 `api_key` /
/// env 任一存在。
fn has_anthropic_credential(s: &AnthropicSection) -> bool {
    !s.credentials.is_empty()
        || s.api_key.as_deref().is_some_and(|k| !k.is_empty())
        || env_has(ENV_ANTHROPIC_API_KEY)
}

fn has_openai_credential(s: &OpenAISection) -> bool {
    !s.credentials.is_empty()
        || s.api_key.as_deref().is_some_and(|k| !k.is_empty())
        || env_has(ENV_OPENAI_API_KEY)
}

fn env_has(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty())
}

fn canonical_provider(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "gpt" => Some("openai"),
        "ollama" | "local" => Some("ollama"),
        // v1.3 SDK:离线 mock provider(e2e / SDK 集成测试用)。
        "mock" | "fake" => Some("mock"),
        _ => None,
    }
}

// ── Subagent Providers:子代理独立凭证构建 ────────────────────────

/// 把 `[subagent_providers.anthropic]` 段展开为 `CredentialPool`,语义与
/// `build_anthropic_pool` 一致但使用 subagent section。
fn build_subagent_anthropic_pool(
    section: &AnthropicSubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = resolve_subagent_anthropic_key(section);
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = AnthropicConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            timeout: default_timeout,
        };
        let client =
            AnthropicClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client: Arc::new(client),
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// Subagent Anthropic section 的 api_key 解析 —— 仅从 TOML 字段读取,
/// 不回退 env(子代理凭证完全由 config 控制)。
fn resolve_subagent_anthropic_key(s: &AnthropicSubagentSection) -> String {
    s.api_key.clone().unwrap_or_default()
}

/// 把 `[subagent_providers.openai]` 段展开为 `CredentialPool`,语义与
/// `build_openai_pool` 一致但使用 subagent section。
fn build_subagent_openai_pool(
    section: &OpenAISubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = resolve_subagent_openai_key(section);
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = OpenAIConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            timeout: default_timeout,
        };
        let client =
            OpenAIClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client: Arc::new(client),
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// Subagent OpenAI section 的 api_key 解析 —— 仅从 TOML 字段读取。
fn resolve_subagent_openai_key(s: &OpenAISubagentSection) -> String {
    s.api_key.clone().unwrap_or_default()
}

/// 把 `[subagent_providers.ollama]` 段展开为 `CredentialPool`,语义与
/// `build_ollama_pool` 一致但使用 subagent section。
fn build_subagent_ollama_pool(
    section: &OllamaSubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    // Subagent Ollama 不回退 env —— 完全由 config 控制。
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: section.api_key.clone().unwrap_or_default(),
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let merged_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(120));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = OllamaConfig {
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            api_key: if cfg.api_key.is_empty() {
                section.api_key.clone()
            } else {
                Some(cfg.api_key.clone())
            },
            keep_alive_secs: section.keep_alive_secs,
            num_ctx: section.num_ctx,
            num_gpu: section.num_gpu,
            timeout: merged_timeout,
            model: section.model.clone(),
        };
        let client =
            OllamaClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        let label = if cfg.label.is_empty() {
            "default".to_string()
        } else {
            cfg.label.clone()
        };
        let weight = if cfg.weight == 0 { 1 } else { cfg.weight };
        entries.push(PoolEntry {
            client: Arc::new(client),
            label,
            weight,
        });
    }
    Ok(Some(CredentialPool { entries }))
}

impl ReflectConfig {
    /// 从 `[subagent_providers]` 段构建独立的 `ModelRegistry`。
    ///
    /// - 若整个 section 缺省 → 返回 `None`,保持默认行为(父子共享 registry)。
    /// - 若任意子 provider 有可用凭证 → 构建并注册到 child registry,返回 `Some(reg)`。
    /// - 所有子 provider 都无凭证 → 返回 `None`。
    pub fn to_child_registry(&self) -> Option<ModelRegistry> {
        let section = self.subagent_providers.as_ref()?;

        let r = ModelRegistry::new();
        let mut any_registered = false;

        if let Some(anth) = &section.anthropic {
            if let Ok(Some(pool)) = build_subagent_anthropic_pool(anth) {
                r.register_pool("anthropic", pool);
                any_registered = true;
            }
        }
        if let Some(oai) = &section.openai {
            if let Ok(Some(pool)) = build_subagent_openai_pool(oai) {
                r.register_pool("openai", pool);
                any_registered = true;
            }
        }
        if let Some(ollama) = &section.ollama {
            if let Ok(Some(pool)) = build_subagent_ollama_pool(ollama) {
                r.register_pool("ollama", pool);
                any_registered = true;
            }
        }

        if any_registered { Some(r) } else { None }
    }
}

// ── MCP (v0.3) ────────────────────────────────────────────────────────────

/// 编译期就绪的 MCP server 配置(`reflect_mcp::McpServerConfig`)——
/// `reflect-config` 不依赖 `reflect-mcp`(避免循环),这里只产出
/// `McpServerConfigShape`,由 `reflect-mcp` 自己的 `From` impl 收尾。
///
/// 之所以在 `reflect-config` 层做校验:
/// 1. 校验可以独立单测,无需启动 rmcp 子进程;
/// 2. `reflect-exec::handle_reload` 在 reload 时也需要复用同一套校验逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfigShape {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    pub timeout: Duration,
}

impl McpServerConfigShape {
    fn from_entry(name: String, entry: &McpServerEntry) -> Result<Self, ConfigError> {
        match entry.transport {
            McpTransport::Stdio => {
                let cmd = entry.command.as_deref().ok_or_else(|| {
                    ConfigError::Build(format!(
                        "[mcp_servers.{name}] type=stdio requires 'command'"
                    ))
                })?;
                Ok(Self {
                    name,
                    transport: McpTransport::Stdio,
                    command: Some(cmd.to_string()),
                    args: entry.args.clone().unwrap_or_default(),
                    env: entry.env.clone().unwrap_or_default(),
                    url: None,
                    headers: HashMap::new(),
                    timeout: Duration::from_millis(
                        entry.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
                    ),
                })
            }
            McpTransport::Http | McpTransport::Sse => {
                let ty = match entry.transport {
                    McpTransport::Sse => "sse",
                    _ => "http",
                };
                let url = entry.url.as_deref().ok_or_else(|| {
                    ConfigError::Build(format!("[mcp_servers.{name}] type={ty} requires 'url'"))
                })?;
                Ok(Self {
                    name,
                    transport: entry.transport,
                    command: None,
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: Some(url.to_string()),
                    headers: entry.headers.clone().unwrap_or_default(),
                    timeout: Duration::from_millis(
                        entry.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
                    ),
                })
            }
        }
    }
}

impl ReflectConfig {
    /// 把 `[mcp_servers.*]` 段编译为强类型列表。
    ///
    /// 任一 entry 校验失败 → 整函数返回 `Err`,由 caller 决定是否 fail-fast
    /// (`reflect-exec::handle_reload` 选择 warn + 跳过单 server,
    /// `bootstrap_m6` 选择 warn + 不启动)。
    pub fn mcp_server_configs(&self) -> Result<Vec<McpServerConfigShape>, ConfigError> {
        let mut out = Vec::with_capacity(self.mcp_servers.servers.len());
        for (name, entry) in &self.mcp_servers.servers {
            out.push(McpServerConfigShape::from_entry(name.clone(), entry)?);
        }
        // HashMap 顺序不稳定,按 server 名排序保证 reload diff 的可重现性。
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// 把 `[lsp_servers.*]` 段编译为强类型列表。
    ///
    /// 校验规则:每个 entry 必须 `file_patterns` 非空(否则 server 不知道
    /// 自己要管哪些文件,无意义);`command` 已由 schema 强制必填。
    ///
    /// 错误路径任一 entry 失败 → 整函数 `Err`,caller 决定 warn + 跳过。
    pub fn lsp_server_configs(&self) -> Result<Vec<LspServerConfigShape>, ConfigError> {
        let mut out = Vec::with_capacity(self.lsp_servers.servers.len());
        for (name, entry) in &self.lsp_servers.servers {
            out.push(LspServerConfigShape::from_entry(name.clone(), entry)?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

#[cfg(test)]
mod tests;

/// 编译期就绪的 LSP server 配置(`reflect_lsp::LspServerConfig` 的薄包装)。
///
/// `reflect-config` 不依赖 `reflect-lsp`,这里只产出
/// `LspServerConfigShape`,由 `reflect-lsp` 的 `TryFrom` impl 收尾
/// (预编译 glob 放 reflect-lsp,因为 `globset` 依赖不传到 config 层)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServerConfigShape {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub patterns: Vec<LspFilePattern>,
    pub root_uri: Option<String>,
    pub initialization_options: Option<serde_json::Value>,
    pub timeout: Duration,
}

impl LspServerConfigShape {
    fn from_entry(name: String, entry: &LspServerEntry) -> Result<Self, ConfigError> {
        if entry.file_patterns.is_empty() {
            return Err(ConfigError::Build(format!(
                "[lsp_servers.{name}] requires at least one file_patterns entry"
            )));
        }
        if entry.command.is_empty() {
            return Err(ConfigError::Build(format!(
                "[lsp_servers.{name}] requires 'command'"
            )));
        }
        Ok(Self {
            name,
            command: entry.command.clone(),
            args: entry.args.clone(),
            env: entry.env.clone(),
            patterns: entry.file_patterns.clone(),
            root_uri: entry.root_uri.clone(),
            initialization_options: entry.initialization_options.clone(),
            timeout: Duration::from_millis(entry.timeout_ms.unwrap_or(DEFAULT_LSP_TIMEOUT_MS)),
        })
    }
}
