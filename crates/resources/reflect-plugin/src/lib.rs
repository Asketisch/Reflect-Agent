//! `reflect-plugin` —— 插件 manifest、lifecycle 与 capability loader。
//!
//! v0 阶段仅暴露数据模型层(`PluginManifest` / `PluginId` /
//! `PluginStatus` / `InstalledPluginsFile` 等)。Phase A 后续小步
//! 加入 `PluginManager::install_local` 与五类 capability loader,
//! 并接入 `reflect-exec::handle_reload` 的 plugins 分支。
//!
//! 关键 schema 字段保持一致,便于跨工具互读 manifest。

pub mod capabilities;
pub mod dependency;
pub mod errors;
pub mod identifier;
pub mod loader;
pub mod manager;
pub mod manifest;
pub mod marketplace;
pub mod state;

// ── 公共 re-export ────────────────────────────────────────────────────

pub use capabilities::{
    CapabilityError, LoadedPlugin,
    agents::LoadedAgent,
    commands::LoadedCommand,
    hooks::LoadedHook,
    mcp::{LoadedMcpServer, McpSource, McpTransportKind},
    skills::LoadedSkill,
};
pub use dependency::{DepResolver, ResolvedDep};
pub use errors::{PluginError, Result};
pub use identifier::{MarketplaceName, PluginId, RESERVED_MARKETPLACE_NAMES};
pub use manager::{InstallReport, PluginInstallFailure, PluginManager};
pub use manifest::{
    HookSpec, MarketplaceManifest, MarketplaceSource, McpServerConfig, McpServerSpec, PluginAuthor,
    PluginDependency, PluginManifest, PluginMarketplaceEntry, PluginRepository, UserConfigOption,
    UserConfigType,
};
pub use reflect_skills::{SkillMeta, SkillsCatalog};
pub use reflect_subagent::{CallSubAgentTool, SubAgentFactory, SubAgentSpec};
pub use state::{
    InstallationEntry, InstalledPluginsFile, KnownMarketplace, KnownMarketplacesFile, PluginScope,
    PluginStatus,
};

/// `Utc::now()` 的 thin wrapper —— 给 CLI / 集成测试在不直接依赖 `chrono`
/// 的前提下构造 `InstallationEntry` / `KnownMarketplace`。
pub fn state_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// `~/.reflect/plugins` —— `PluginManager` 默认根目录。
///
/// `HOME` 未设置时返回 `None` —— 调用方应把 `None` 当成"无法定位 plugins 目录"
/// 而不是 panic(避免 init 进程 + daemon 进程对 `$HOME` 的不一致预期)。
pub fn default_plugins_root() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join(".reflect").join("plugins"))
}

/// `~/.reflect/plugins/marketplaces.json` —— known marketplaces 持久化点。
pub fn default_marketplaces_path() -> Option<std::path::PathBuf> {
    default_plugins_root().map(|p| p.join("marketplaces.json"))
}
