//! `Credential` — 单个 API 凭证 + 元数据。
//!
//! 每个 provider(anthropic / openai / ollama)在 `ModelRegistry` 中以
//! `CredentialPool` 形式持有一组 `Credential`,每个 `Credential` 对应一个
//! `Arc<dyn ModelClient>`。`label` 是用户自定义的诊断标识(出现在
//! `StreamErrorEvent.credential_label` 与 TUI status bar 中),`weight`
//! 决定 round-robin 比例,`cooldown_override` 允许对单个 credential
//! 调冷却时长(覆盖 `RoutingPolicy` 全局默认)。
//!
//! v1.0 多 Provider 路由:这是 `[[<provider>.credentials]]` TOML 数组
//! 形态的镜像;旧 `api_key = "..."` 单值在 builder 阶段自动 wrap 为
//! `label = "default"` 的单条 credential。

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// 用户标签,如 `"work"` / `"personal"` / `"default"`(旧配置 wrap 后)。
    pub label: String,
    /// API key 密文。`reflect-config` 解析时可能承载 env 引用。
    pub api_key: String,
    /// 覆盖 section 级 `base_url`(Ollama 本地 server URL 等)。
    pub base_url: Option<String>,
    /// round-robin 权重,默认 1。`weight = 0` 的 credential 会被跳过。
    pub weight: u32,
    /// 单 credential 冷却时长覆盖;`None` 走 `RoutingPolicy` 全局默认。
    pub cooldown_override: Option<Duration>,
}

impl Credential {
    /// 用给定的 label + api_key 构造,其余字段走默认(`weight = 1`)。
    pub fn new(label: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            api_key: api_key.into(),
            base_url: None,
            weight: 1,
            cooldown_override: None,
        }
    }
}
