//! 插件持久化状态。
//!
//! 关键设计:
//! - **同 plugin id 可装多 scope** —— `Record<id, [InstallationEntry]>`
//!   数组形态,允许 user / project 同时启用同一 plugin(不同 scope precedence)。
//! - **enabled flag 与 installation 分离** —— `installed_plugins.json` 只
//!   描述"装了什么",`enabled_plugins` 在 `~/.reflect/config.toml` 描述
//!   "启用什么"。`scope precedence = local > project > user > managed`。
//! - **orphan 文件** —— 老版本 cache 目录写 `.orphaned_at`,7 天后才 GC,
//!   期间仍可被并发 session 访问。

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identifier::{MarketplaceName, PluginId};
use crate::manifest::MarketplaceSource;

/// 插件生命周期状态。
///
/// `Installed` 是初装未启用的中间态;`Enabled` / `Disabled` 是用户
/// 显式选择的运行态;`Error` 是 load 失败后的兜底态(用户可重新
/// enable 触发 retry)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PluginStatus {
    /// 已装但未启用(出现在 installed_plugins.json 但不在 enabled_plugins)。
    Installed,
    /// 启用,所有能力已挂载。
    Enabled,
    /// 禁用,所有能力已反注册(但文件还在 cache)。
    Disabled,
    /// 加载失败 —— 用户可手动重试。
    Error { reason: String },
}

impl PluginStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, PluginStatus::Enabled)
    }

    pub fn is_inert(&self) -> bool {
        matches!(self, PluginStatus::Disabled | PluginStatus::Installed)
    }
}

/// 插件 scope —— 决定谁有权管理它。
///
/// `Managed` 由企业 `~/.reflect/managed-settings.json` pin,
/// 用户不能 disable / uninstall。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginScope {
    /// 企业级,只读,管理员 pin。
    Managed,
    /// 用户级(`~/.reflect/config.toml`)。
    User,
    /// 项目级(`./.reflect/settings.toml`,v0 不支持)。
    Project,
    /// 会话级(临时,`-`flag 注入,关闭即失)。
    Local,
}

impl PluginScope {
    /// 返回 precedence 数值(大者覆盖小者)。
    pub fn precedence(&self) -> u8 {
        match self {
            PluginScope::Local => 4,
            PluginScope::Project => 3,
            PluginScope::User => 2,
            PluginScope::Managed => 1,
        }
    }
}

/// 单次安装的记录 —— 同 plugin id 可在多个 scope 下共存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationEntry {
    pub scope: PluginScope,
    pub install_path: PathBuf,
    pub version: String,
    pub installed_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit_sha: Option<String>,
    /// manifest + payload 的 sha256,load 时校验不一致 → warn(不阻断)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_sha256: Option<String>,
}

/// `installed_plugins.json` V2 顶层 —— 同 plugin id 数组允许多 scope 多 version。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPluginsFile {
    /// Schema 版本号,固定为 2。
    pub version: u32,
    pub plugins: BTreeMap<PluginId, Vec<InstallationEntry>>,
}

impl InstalledPluginsFile {
    /// 构造空文件。
    pub fn new() -> Self {
        Self {
            version: 2,
            plugins: BTreeMap::new(),
        }
    }

    /// 追加一条 entry;同 `(id, scope, install_path)` 已存在则覆盖。
    pub fn upsert(&mut self, id: PluginId, entry: InstallationEntry) {
        let entries = self.plugins.entry(id).or_default();
        // 同 scope + 同 path 视为同一安装,覆盖;否则追加。
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| e.scope == entry.scope && e.install_path == entry.install_path)
        {
            *existing = entry;
        } else {
            entries.push(entry);
        }
    }

    /// 移除所有 `(id, scope)` 条目(通常用于 uninstall 时清掉某个 scope)。
    /// 返回实际移除的 entry 数。
    pub fn remove_scope(&mut self, id: &PluginId, scope: PluginScope) -> usize {
        let Some(entries) = self.plugins.get_mut(id) else {
            return 0;
        };
        let before = entries.len();
        entries.retain(|e| e.scope != scope);
        let removed = before - entries.len();
        if entries.is_empty() {
            self.plugins.remove(id);
        }
        removed
    }

    /// 列出某 plugin id 的所有 entries(按 scope precedence 降序)。
    pub fn entries_for(&self, id: &PluginId) -> Vec<&InstallationEntry> {
        let mut out: Vec<&InstallationEntry> = self
            .plugins
            .get(id)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        out.sort_by_key(|e| std::cmp::Reverse(e.scope.precedence()));
        out
    }
}

/// Marketplace 安装记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownMarketplace {
    pub source: MarketplaceSource,
    pub install_location: PathBuf,
    pub last_updated: DateTime<Utc>,
    /// true → 启动时后台刷新。
    #[serde(default)]
    pub auto_update: bool,
}

/// `known_marketplaces.json` 顶层。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownMarketplacesFile {
    pub marketplaces: BTreeMap<MarketplaceName, KnownMarketplace>,
}

impl KnownMarketplacesFile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, name: MarketplaceName, m: KnownMarketplace) {
        self.marketplaces.insert(name, m);
    }

    pub fn remove(&mut self, name: &MarketplaceName) -> Option<KnownMarketplace> {
        self.marketplaces.remove(name)
    }
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(scope: PluginScope, path: &str) -> InstallationEntry {
        InstallationEntry {
            scope,
            install_path: PathBuf::from(path),
            version: "1.0.0".into(),
            installed_at: Utc::now(),
            last_updated: Utc::now(),
            git_commit_sha: None,
            checksum_sha256: None,
        }
    }

    #[test]
    fn installed_file_upsert_dedup() {
        let mut f = InstalledPluginsFile::new();
        let id = PluginId::parse("foo@bar").unwrap();
        f.upsert(id.clone(), entry(PluginScope::User, "/a"));
        f.upsert(id.clone(), entry(PluginScope::User, "/a")); // 同 scope+path 应覆盖
        assert_eq!(f.plugins[&id].len(), 1);
        f.upsert(id.clone(), entry(PluginScope::Project, "/a")); // 不同 scope 追加
        assert_eq!(f.plugins[&id].len(), 2);
    }

    #[test]
    fn installed_file_remove_scope() {
        let mut f = InstalledPluginsFile::new();
        let id = PluginId::parse("foo@bar").unwrap();
        f.upsert(id.clone(), entry(PluginScope::User, "/a"));
        f.upsert(id.clone(), entry(PluginScope::Project, "/a"));
        let removed = f.remove_scope(&id, PluginScope::User);
        assert_eq!(removed, 1);
        assert_eq!(f.plugins[&id].len(), 1);
        assert_eq!(f.plugins[&id][0].scope, PluginScope::Project);
    }

    #[test]
    fn installed_file_remove_last_scope_drops_entry() {
        let mut f = InstalledPluginsFile::new();
        let id = PluginId::parse("foo@bar").unwrap();
        f.upsert(id.clone(), entry(PluginScope::User, "/a"));
        f.remove_scope(&id, PluginScope::User);
        assert!(f.plugins.is_empty());
    }

    #[test]
    fn installed_file_entries_for_sorts_by_precedence() {
        let mut f = InstalledPluginsFile::new();
        let id = PluginId::parse("foo@bar").unwrap();
        f.upsert(id.clone(), entry(PluginScope::User, "/a"));
        f.upsert(id.clone(), entry(PluginScope::Local, "/b"));
        f.upsert(id.clone(), entry(PluginScope::Managed, "/c"));
        let out = f.entries_for(&id);
        assert_eq!(out[0].scope, PluginScope::Local);
        assert_eq!(out[1].scope, PluginScope::User);
        assert_eq!(out[2].scope, PluginScope::Managed);
    }

    #[test]
    fn serde_round_trip_installed_file() {
        let mut f = InstalledPluginsFile::new();
        let id = PluginId::parse("foo@bar").unwrap();
        f.upsert(id.clone(), entry(PluginScope::User, "/a"));
        let json = serde_json::to_string_pretty(&f).unwrap();
        let back: InstalledPluginsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn serde_round_trip_known_marketplaces() {
        let mut f = KnownMarketplacesFile::new();
        let name = MarketplaceName::parse("official").unwrap();
        f.upsert(
            name.clone(),
            KnownMarketplace {
                source: crate::manifest::MarketplaceSource::Github {
                    repo: "anthropics/claude-plugins-official".into(),
                    r#ref: None,
                    sha: None,
                },
                install_location: PathBuf::from("/tmp/official"),
                last_updated: Utc::now(),
                auto_update: true,
            },
        );
        let json = serde_json::to_string(&f).unwrap();
        let back: KnownMarketplacesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn plugin_status_active_inert() {
        assert!(PluginStatus::Enabled.is_active());
        assert!(!PluginStatus::Enabled.is_inert());
        assert!(PluginStatus::Disabled.is_inert());
        assert!(PluginStatus::Installed.is_inert());
        assert!(!PluginStatus::Error { reason: "x".into() }.is_active());
    }

    #[test]
    fn scope_precedence_ordering() {
        assert!(PluginScope::Local.precedence() > PluginScope::Project.precedence());
        assert!(PluginScope::Project.precedence() > PluginScope::User.precedence());
        assert!(PluginScope::User.precedence() > PluginScope::Managed.precedence());
    }

    #[test]
    fn empty_files_serde() {
        let f = InstalledPluginsFile::new();
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"version\":2"));
        let back: InstalledPluginsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
