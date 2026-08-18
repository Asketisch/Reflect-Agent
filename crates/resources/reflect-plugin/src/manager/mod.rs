//! `PluginManager` —— 本地 install / uninstall / list 的核心骨架。
//!
//! v0(Phase A)范围:
//! - `install_local(path)` —— 读 manifest、校验、复制到 cache 目录、
//!   写 `installed_plugins.json` 条目
//! - `uninstall(id, scope)` —— 移除 JSON 条目 + 删除 cache 子树
//! - `list_installed()` / `iter_installed()` —— UI / 测试用视图
//! - `save()` / `load()` —— 持久化 round-trip
//!
//! Phase A 不做:
//! - 5 类 capability 实际挂载(load/unload 留 Phase B,因为这要修改
//!   `reflect-tools` / `reflect-hooks` / `reflect-mcp` / `reflect-skills`
//!   / `reflect-subagent` 五个 crate,工作量与风险都更大)
//! - marketplace 拉取(Phase D)
//! - HTTP / Git / sha 校验(Phase E)
//! - 依赖解析(Phase F)
//!
//! 设计要点:
//! - `plugins_root` 默认 `~/.reflect/plugins`,但所有 API 接受
//!   `plugins_root: impl Into<PathBuf>`,便于测试用 `tempfile::tempdir`。
//! - install_path 缓存布局 `<root>/cache/<marketplace>/<plugin>/<version>/`。
//! - copy 用自定义 `copy_dir_all`(避免引入额外依赖)。

mod fs_util;
mod report;

#[cfg(test)]
mod tests;

pub use report::{InstallReport, PluginInstallFailure};

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::errors::{PluginError, Result};
use crate::identifier::{MarketplaceName, PluginId};
use crate::manifest::{MarketplaceManifest, PluginManifest};
use crate::state::{InstallationEntry, InstalledPluginsFile, KnownMarketplacesFile, PluginScope};

use fs_util::copy_dir_all;

/// `installed_plugins.json` 在 `plugins_root` 下的文件名。
const INSTALLED_FILE: &str = "installed_plugins.json";

/// plugin 缓存的根目录(相对 `plugins_root`)。
const CACHE_DIR: &str = "cache";

/// 线程安全的插件管理器。
///
/// v0 仅持有 `installed_plugins.json` 的内存视图与 `plugins_root`。
/// 后续 Phase B 会再持有 ToolRegistry / HookEngine / McpConnectionManager
/// 等引用,用于 load/unload。
pub struct PluginManager {
    plugins_root: PathBuf,
    state: InstalledPluginsFile,
}

impl PluginManager {
    /// 构造 manager —— **不**触发 IO。`load()` 单独负责读盘。
    pub fn new(plugins_root: impl Into<PathBuf>) -> Self {
        Self {
            plugins_root: plugins_root.into(),
            state: InstalledPluginsFile::new(),
        }
    }

    /// 从 `plugins_root/installed_plugins.json` 加载状态。
    /// 文件不存在则视为空 manager,返回 `Ok`。
    pub fn load(plugins_root: impl Into<PathBuf>) -> Result<Self> {
        let plugins_root = plugins_root.into();
        let path = plugins_root.join(INSTALLED_FILE);
        let state = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|e| PluginError::StateIo {
                path: path.clone(),
                source: e,
            })?;
            serde_json::from_str(&text).map_err(|e| PluginError::StateParse(e.to_string()))?
        } else {
            InstalledPluginsFile::new()
        };
        Ok(Self {
            plugins_root,
            state,
        })
    }

    /// 拿到 plugins 根目录(只读)。
    pub fn plugins_root(&self) -> &Path {
        &self.plugins_root
    }

    /// 拿到当前 state 的只读引用。
    pub fn state(&self) -> &InstalledPluginsFile {
        &self.state
    }

    /// 持久化 state 到 `installed_plugins.json`。原子写:写到
    /// `installed_plugins.json.tmp` 再 rename,避免半截文件污染。
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.plugins_root).map_err(|e| PluginError::StateIo {
            path: self.plugins_root.clone(),
            source: e,
        })?;
        let target = self.plugins_root.join(INSTALLED_FILE);
        let tmp = self.plugins_root.join(format!("{INSTALLED_FILE}.tmp"));
        let json = serde_json::to_string_pretty(&self.state)
            .map_err(|e| PluginError::StateParse(e.to_string()))?;
        fs::write(&tmp, json).map_err(|e| PluginError::StateIo {
            path: tmp.clone(),
            source: e,
        })?;
        fs::rename(&tmp, &target).map_err(|e| PluginError::StateIo {
            path: target,
            source: e,
        })?;
        Ok(())
    }

    /// 从本地路径安装 plugin。
    ///
    /// 流程:
    /// 1. 在 `local_path` 下找 manifest(`plugin.toml` 或 `.claude-plugin/plugin.json`)
    /// 2. 校验 manifest.name 符合 kebab-case
    /// 3. 解析出 version(缺省 `"0.0.0"`)
    /// 4. 计算 `plugin_id = "<name>@<marketplace>"`、`install_path`
    /// 5. 复制 `local_path` 内容到 `install_path`
    /// 6. upsert InstallationEntry,save()
    ///
    /// **Phase D 改动**:`marketplace` 参数决定 plugin id 的 marketplace 段,
    /// 取代之前硬编码 `@inline`。CLI 端本地 install 仍传 `MarketplaceName::inline()`。
    ///
    /// 返回 `(PluginId, InstallationEntry)`。
    pub fn install_local(
        &mut self,
        local_path: impl AsRef<Path>,
        marketplace: &MarketplaceName,
        scope: PluginScope,
    ) -> Result<(PluginId, InstallationEntry)> {
        let local_path = local_path.as_ref();
        let manifest_path =
            PluginManifest::find_in_dir(local_path).ok_or_else(|| PluginError::ManifestParse {
                path: local_path.to_path_buf(),
                message: format!(
                    "未在 {} 下找到 plugin.toml 或 .claude-plugin/plugin.json",
                    local_path.display()
                ),
            })?;
        let manifest = PluginManifest::from_path(&manifest_path)?;
        // ID 构造:Phase D 起,mkt 段由参数决定。
        // 注意:`inline` 是保留 marketplace 名(`PluginId::new` 拒绝),
        // 但 `MarketplaceName::inline()` 是合法来源 —— 此时用 `PluginId::inline`
        // 走特例构造器。
        let id = if marketplace.is_inline() {
            PluginId::inline(&manifest.name)?
        } else {
            PluginId::new(&manifest.name, marketplace.as_str())?
        };
        let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".into());

        // 已装检查 —— 同 (id, scope) 已存在则报错。
        if self.state.entries_for(&id).iter().any(|e| e.scope == scope) {
            return Err(PluginError::AlreadyInstalled(id.to_string()));
        }

        let install_path = self.cache_path_for(&id, &version);
        if install_path.exists() {
            return Err(PluginError::InstallDirConflict(install_path));
        }
        copy_dir_all(local_path, &install_path).map_err(|e| PluginError::StateIo {
            path: install_path.clone(),
            source: e,
        })?;

        let now = Utc::now();
        let entry = InstallationEntry {
            scope,
            install_path,
            version,
            installed_at: now,
            last_updated: now,
            git_commit_sha: None,
            checksum_sha256: None,
        };
        self.state.upsert(id.clone(), entry.clone());
        self.save()?;
        Ok((id, entry))
    }

    /// 从 marketplace cache 装一个 plugin 及其依赖 closure。
    ///
    /// **Phase F 行为**:
    /// 1. 校验 `<marketplace_cache_root>/.claude-plugin/marketplace.json` 存在
    ///    (缺则 `Err(MarketplaceManifestNotFound)`,fail-fast)。
    /// 2. 走 `DepResolver::resolve` —— DFS 解析 `dependencies`,**后序**装,
    ///    检测环 / 缺依赖 / 跨 marketplace 等异常,**不中断**地把失败 push
    ///    到 `report.failed`,全部走完后返回 `InstallReport`。
    ///
    /// 调用方根据 `report.is_success()` 决定是否 `process::exit(1)`。
    ///
    /// `known` 用于跨 marketplace dep 解析 —— 调用方传
    /// `load_known_marketplaces(&marketplaces_path)?` 即可;**不**被此函数
    /// 持久化修改(`DepResolver` 借用 `known` 作只读视图)。
    pub fn install_from_marketplace(
        &mut self,
        known: &KnownMarketplacesFile,
        marketplace_cache_root: impl AsRef<Path>,
        marketplace_name: &MarketplaceName,
        plugin_name: &str,
        scope: PluginScope,
    ) -> Result<InstallReport> {
        let marketplace_cache_root = marketplace_cache_root.as_ref();
        // 必须有 manifest 才能走 dep resolver;缺则 fail-fast。
        let _manifest_path =
            MarketplaceManifest::find_in_dir(marketplace_cache_root).ok_or_else(|| {
                PluginError::MarketplaceManifestNotFound(format!(
                    "{}/.claude-plugin/marketplace.json",
                    marketplace_cache_root.display()
                ))
            })?;

        let root = crate::dependency::ResolvedDep {
            name: plugin_name.to_string(),
            marketplace: marketplace_name.clone(),
            cache_root: marketplace_cache_root.to_path_buf(),
        };

        let mut resolver = crate::dependency::DepResolver::new(known, self);
        let report = resolver.resolve(root, scope);
        Ok(report)
    }

    /// 卸载 plugin —— 移除 `(id, scope)` 条目,删除对应 install_path。
    ///
    /// Managed scope 拒绝(用户不能改企业 pin 的插件)。
    /// 删 cache 失败不阻断 JSON 移除,只 warn —— 这是 best-effort,
    pub fn uninstall(&mut self, id: &PluginId, scope: PluginScope) -> Result<()> {
        if scope == PluginScope::Managed {
            return Err(PluginError::ManagedLocked(id.to_string()));
        }
        let entries = self.state.entries_for(id);
        let target = entries
            .iter()
            .find(|e| e.scope == scope)
            .ok_or_else(|| PluginError::NotInstalled(id.to_string()))?
            .install_path
            .clone();
        let removed = self.state.remove_scope(id, scope);
        if removed == 0 {
            return Err(PluginError::NotInstalled(id.to_string()));
        }
        self.save()?;
        if target.exists()
            && let Err(e) = fs::remove_dir_all(&target)
        {
            tracing::warn!(
                install_path = %target.display(),
                error = %e,
                "uninstall: 移除 cache 目录失败,JSON 已清理"
            );
        }
        Ok(())
    }

    /// 列出所有已装 plugin 的 (id, entry) 对 —— 按 id 字典序。
    pub fn list_installed(&self) -> Vec<(PluginId, InstallationEntry)> {
        let mut out: Vec<(PluginId, InstallationEntry)> = Vec::new();
        for (id, entries) in &self.state.plugins {
            // 同 plugin id 的多 scope entries 都列出 —— 调用方按 scope 过滤。
            for entry in entries {
                out.push((id.clone(), entry.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 按 id 找 entries —— 复用 `InstalledPluginsFile::entries_for`,
    /// 已按 scope precedence 排序。
    pub fn entries_for(&self, id: &PluginId) -> Vec<&InstallationEntry> {
        self.state.entries_for(id)
    }

    /// 计算 plugin 的 install_path。
    ///
    /// 布局:`<plugins_root>/cache/<marketplace>/<plugin>/<version>/`
    pub fn cache_path_for(&self, id: &PluginId, version: &str) -> PathBuf {
        self.plugins_root
            .join(CACHE_DIR)
            .join(id.marketplace())
            .join(id.name())
            .join(version)
    }

    // ── v1.0.0-rc2:生命周期 helpers ──────────────────────────────────

    /// 刷新 `(id, scope)` 条目的 `last_updated` 字段并落盘。
    ///
    /// v1.0.0-rc2 简化决策:**启用态的真实权威表**是 `ReflectConfig.plugins.enabled_plugins`,
    /// 这里只更新 `last_updated` 供 UI 显示用。CLI / TUI 在改 enabled_plugins
    /// 后**不**调用 `touch`(避免双重写盘);只有用户显式 upgrade / reinstall
    /// 时才会走这里。
    ///
    /// 返回更新后的 entry 引用 —— 出错(没装)返回 `NotInstalled`。
    pub fn touch(&mut self, id: &PluginId, scope: PluginScope) -> Result<()> {
        let entries = self
            .state
            .plugins
            .get_mut(id)
            .ok_or_else(|| PluginError::NotInstalled(id.to_string()))?;
        let target = entries
            .iter_mut()
            .find(|e| e.scope == scope)
            .ok_or_else(|| PluginError::NotInstalled(id.to_string()))?;
        target.last_updated = Utc::now();
        self.save()
    }

    /// 列出 entries 并附带"该 id 是否在 `enabled_plugins` 集合中"标志。
    ///
    /// 排序规则同 `list_installed`(按 id 字典序);若同 id 多 scope,各
    /// scope 独立标 enabled(`id` 进 enabled 集合时,所有 scope 都标 true
    /// —— 实际启用态由 runtime 决定,本方法只镜像 config 表的字符串包含关系)。
    pub fn list_with_enabled(
        &self,
        enabled: &HashSet<String>,
    ) -> Vec<(PluginId, InstallationEntry, bool)> {
        let mut out: Vec<(PluginId, InstallationEntry, bool)> = Vec::new();
        for (id, entries) in &self.state.plugins {
            let is_enabled = enabled.contains(id.as_str());
            for entry in entries {
                out.push((id.clone(), entry.clone(), is_enabled));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 按 id 在 installed_plugins.json 中查找 entries —— 用于 `info` / `show`。
    /// 若 id 不存在则返回 `None`(调用方用 `NotInstalled` 错误格式输出)。
    pub fn lookup_entry(&self, id: &PluginId) -> Option<&InstallationEntry> {
        self.state.entries_for(id).into_iter().next()
    }
}
