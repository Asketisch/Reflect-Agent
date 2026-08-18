//! 插件系统配置 section。
//!
//! 包含:
//! - `PluginMarketplaceConfig` / `PluginMarketplaceKind` — marketplace 源定义
//! - `PluginsSection` — 已启用插件列表 + marketplace 源

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ── Plugins (v1.0.0-rc2) ───────────────────────────────────────────

/// 单个 plugin 在 `[plugins.marketplaces.<name>]` 下的源定义。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginMarketplaceConfig {
    #[serde(rename = "type", default)]
    pub kind: PluginMarketplaceKind,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub auto_update: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginMarketplaceKind {
    #[default]
    Directory,
    File,
    Github,
    Url,
}

/// `[plugins]` 段 —— 描述已启用的 plugin 与已知的 marketplace 源。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginsSection {
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub marketplaces: HashMap<String, PluginMarketplaceConfig>,
}

impl PluginsSection {
    /// 检查某个 plugin id 是否启用。
    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        self.enabled_plugins.iter().any(|p| p == plugin_id)
    }
}
