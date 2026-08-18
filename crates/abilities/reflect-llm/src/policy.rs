//! 角色 → spec slot 路由策略。
//!
//! `RoutingPolicy` 是 v1.0 多 Provider 路由的 per-role 路由核心:每个
//! `Role`(`Main` / `Compact` / `Subagent(name)`)对应一个 `SpecSlot`,
//! `SpecSlot` 描述一个 primary + 多个 fallback + 对应权重。`model_call`
//! 入口用 `policy.resolve(Role::Main)` 拿到 slot.primary 作为初始
//! spec,失败时由 `ModelRegistry::next_for` 在 pool 内自动切下一个
//! credential,跨 credential 全失败后由 `model_call` 走 slot.fallbacks。
//!
//! 全局 cooldown 默认值给 `model_call` 的 `classify_retry` 决策用
//! (Phase 2 接),`max_attempts` 限制单轮总尝试次数,默认 16。
//!
//! v1.0 Phase 3:从 `ReflectConfig.routing` TOML 段构建;v1.0 Phase 1
//! 暂用 `RoutingPolicy::default()`,`main.primary` 为空时由 caller
//! 用 `cfg.model_for(provider)` 兜底。

use std::time::Duration;

/// 跨角色共享的路由策略。`Clone` 便宜(单 String + 几个 `Vec`),用
/// `Arc<RoutingPolicy>` 共享给 `AgentConfig` / `LlmSummarizer` /
/// `SubAgentFactory`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicy {
    /// 主对话 turn 的 spec 槽位。
    pub main: SpecSlot,
    /// 压缩阶段(`LlmSummarizer`)的 spec 槽位;通常用便宜模型。
    pub compact: SpecSlot,
    /// 子 agent(`SubAgentFactory::spawn`)的 spec 槽位。
    pub subagent: SpecSlot,
    /// 角色名前缀,留扩展用;`Role::Subagent("researcher")` 配合
    /// `subagent_prefix = "subagent"` 形成 `"subagent/researcher"`。
    pub subagent_prefix: String,
    /// 单轮 LLM 调用的最大尝试次数(包括 cooldown / failover)。
    /// 默认 16,够 `candidates * 2` 跑两轮。
    pub max_attempts: u32,
    /// 429 全局默认 cooldown(credential 级 `cooldown_override` 优先)。
    pub default_cooldown_rate_limited: Duration,
    /// 401 全局默认 cooldown(默认 1 小时,Auth 不自愈)。
    pub default_cooldown_auth: Duration,
    /// 5xx 全局默认 cooldown。
    pub default_cooldown_5xx: Duration,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            main: SpecSlot::default(),
            compact: SpecSlot::default(),
            subagent: SpecSlot::default(),
            subagent_prefix: "subagent".to_string(),
            max_attempts: 16,
            default_cooldown_rate_limited: Duration::from_secs(60),
            default_cooldown_auth: Duration::from_secs(3600),
            default_cooldown_5xx: Duration::from_secs(60),
        }
    }
}

/// 单角色的 spec 列表:`primary` 优先,失败后依次试 `fallbacks`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpecSlot {
    /// `"provider/model"` 形式 spec,如 `"anthropic/claude-3-5-sonnet-latest"`。
    pub primary: String,
    /// fallback spec 列表;`weights` 等长,默认全 1。
    pub fallbacks: Vec<String>,
    /// 与 `fallbacks` 一一对应的 round-robin 权重。
    pub weights: Vec<u32>,
}

impl SpecSlot {
    /// 给定 slot primary 起点(由 `model_for` 算出),构造仅含 primary
    /// 的 slot。常用于 `RoutingPolicy::default()` 后由 `bootstrap_m4`
    /// 注入实际 spec。
    pub fn with_primary(primary: impl Into<String>) -> Self {
        Self {
            primary: primary.into(),
            fallbacks: Vec::new(),
            weights: Vec::new(),
        }
    }
}

/// 三种角色:`Main`(主对话 turn)、`Compact`(压缩阶段)、
/// `Subagent(name)`(子 agent;`name` 来自 `SubAgentSpec.name`)。
///
/// v1.0 Phase 3 时 `Subagent` 仍共享 `policy.subagent` slot;
/// Phase 4 之后可扩展为每个 subagent 一个独立 slot。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Main,
    Compact,
    Subagent(String),
}

impl RoutingPolicy {
    /// 给定角色返回对应 slot 引用。`Subagent(_)` 统一走 `self.subagent`,
    /// 真正的 per-spec 选择留给 `SpecSlot` 内部权重(Phase 4 可扩展为
    /// per-subagent 独立 slot)。
    pub fn resolve(&self, role: Role) -> &SpecSlot {
        match role {
            Role::Main => &self.main,
            Role::Compact => &self.compact,
            Role::Subagent(_) => &self.subagent,
        }
    }
}
