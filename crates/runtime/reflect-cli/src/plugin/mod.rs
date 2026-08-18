//! `reflect plugin ...` —— 插件 install / list / enable / disable / uninstall / info / show。
//!
//! v1.0.0-rc2 范围:
//! - `install <path>` 走 `PluginManager::install_local`,写真实表(`installed_plugins.json`)。
//! - `enable` / `disable` 改 `ReflectConfig.plugins.enabled_plugins`(权威启用表)。
//! - `list` / `info` / `show` 联表(安装 + 启用)显示。
//! - `uninstall` 双写:install 事实表 + enabled 列表。
//! - `marketplace add/ls/remove`:
//!   - `add` 走 `MarketplaceFetchRouter` 真 fetch(Git / File / Directory 三种),
//!     Github / Url 报 `Phase E stub`。
//!   - `remove` 真删 install_location cache 目录。
//!
//! ## 模块拆分
//! - [`marketplace`] — marketplace add / ls / remove / refresh
//! - [`tests`] — 单测
//!
//! 与 `reflect mcp ...` 的设计对称;不触发跨进程热重载,末尾会提示用户
//! 重启 `reflect exec`(交互式 TUI 见独立仓库 Reflect-TUI)。

pub mod marketplace;
#[cfg(test)]
mod tests;

// 重导出 marketplace 公共函数,保持向后兼容。
pub use marketplace::{marketplace_add, marketplace_ls, marketplace_refresh, marketplace_remove};
// `load_known_marketplaces` 供本模块的 `install` 使用。
use marketplace::load_known_marketplaces;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use reflect_config::{ReflectConfig, default_config_path, load_default};
use reflect_plugin::state::PluginScope;
#[cfg(test)]
use reflect_plugin::{InstalledPluginsFile, PluginManifest};
use reflect_plugin::{PluginId, PluginManager};

use crate::login;

/// `~/.reflect/plugins` —— `PluginManager` 默认根目录。
fn default_plugins_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".reflect").join("plugins"))
}

/// `~/.reflect/plugins/marketplaces.json` —— Phase D 的 known marketplaces 持久化点。
fn default_marketplaces_path() -> Option<PathBuf> {
    default_plugins_root().map(|p| p.join("marketplaces.json"))
}

/// 一次性 load manager + config,失败报错并退出。
fn load_manager_and_config() -> anyhow::Result<(PluginManager, ReflectConfig)> {
    let root =
        default_plugins_root().ok_or_else(|| anyhow!("HOME unset; cannot locate plugins dir"))?;
    let mgr = PluginManager::load(&root)
        .with_context(|| format!("failed to load PluginManager at {}", root.display()))?;
    let cfg = load_default();
    Ok((mgr, cfg))
}

/// 把 cfg 写回 `~/.reflect/config.toml`(复用 `login::write_config_to`)。
fn save_config(cfg: &ReflectConfig) -> anyhow::Result<()> {
    let path =
        default_config_path().ok_or_else(|| anyhow!("HOME unset; cannot locate config dir"))?;
    login::write_config_to(cfg, &path)?;
    Ok(())
}

/// `reflect plugin install <path|plugin-id>` —— 复制本地 plugin 到 cache 并写 installed_plugins.json。
///
/// Phase D 自动分流:
/// - `<name>@<marketplace>` 形态 → 走 `install_from_marketplace`
///   (前提:`marketplace add` 已注册,plugin 在 marketplace manifest 里)
/// - 其他 → 走 `install_local`,默认 `marketplace = inline`
pub async fn install(path: &Path, scope: PluginScope) -> anyhow::Result<()> {
    let root =
        default_plugins_root().ok_or_else(|| anyhow!("HOME unset; cannot locate plugins dir"))?;
    let mut mgr = PluginManager::load(&root)?;

    // 形态判断:`<name>@<marketplace>` 走 marketplace 路径。
    // 注意:`path` 在 clap 里是 PathBuf,但 plugin id 形态如 `demo@smoke-mkt`
    // 也能被 PathBuf 接受(只含 ASCII),所以我们用 OsStr 比较 + 解析。
    let id_str = path.to_string_lossy();
    if let Some((name, marketplace)) = id_str.split_once('@') {
        // 解析 plugin id(保留名允许 —— plugin id @ 后可以是 inline/builtin/任何 mkt)。
        let mkt_name = reflect_plugin::MarketplaceName::parse(marketplace)
            .map_err(|e| anyhow!("invalid marketplace in '{id_str}': {e}"))?;
        // 从 known_marketplaces.json 拿 install_location。
        let mp_path = default_marketplaces_path()
            .ok_or_else(|| anyhow!("HOME unset; cannot resolve marketplaces.json path"))?;
        let known = load_known_marketplaces(&mp_path)?;
        let entry = known
            .marketplaces
            .get(&mkt_name)
            .ok_or_else(|| anyhow!("marketplace '{marketplace}' not registered; run `reflect plugin marketplace add {marketplace} --from ...` first"))?;
        let cache_root = entry.install_location.clone();
        let report = mgr
            .install_from_marketplace(&known, &cache_root, &mkt_name, name, scope)
            .with_context(|| {
                format!("install from marketplace '{marketplace}' failed for '{name}'")
            })?;
        // Phase F:多行聚合报告,失败 exit 1。
        render_install_report(&report, marketplace);
        if !report.is_success() {
            std::process::exit(1);
        }
        return Ok(());
    }

    // 默认:本地路径 → install_local,marketplace = inline。
    let (id, entry) = mgr
        .install_local(path, &reflect_plugin::MarketplaceName::inline(), scope)
        .with_context(|| format!("install failed for {}", path.display()))?;
    println!("installed {} -> {}", id, entry.install_path.display());
    println!("Restart `reflect exec` to load capabilities.");
    Ok(())
}

/// 渲染 `InstallReport` 到 stdout —— Phase F 多行聚合(成功 / 失败分块)。
///
/// 失败块非空时调用方应 `std::process::exit(1)`,本函数只负责打印。
fn render_install_report(report: &reflect_plugin::InstallReport, marketplace: &str) {
    println!(
        "✓ installed {} (from marketplace '{}'):",
        report.summary(),
        marketplace
    );
    for (id, entry) in &report.installed {
        println!("  + {} -> {}", id, entry.install_path.display());
    }
    if !report.failed.is_empty() {
        println!("✗ failed {}:", report.failed.len());
        for f in &report.failed {
            println!("  - {} : {}", f.target, f.error);
        }
    }
    if !report.installed.is_empty() {
        println!("Restart `reflect exec` to load capabilities.");
    }
}

/// `reflect plugin list` —— 表格列出已装 + 是否启用。
pub fn list() -> anyhow::Result<()> {
    let (mgr, cfg) = load_manager_and_config()?;
    let enabled: HashSet<String> = cfg.plugins.enabled_plugins.iter().cloned().collect();
    let rows = mgr.list_with_enabled(&enabled);
    if rows.is_empty() {
        println!("(no plugins installed; run `reflect plugin install <path>` to add one)");
        return Ok(());
    }
    println!(
        "{:<28}  {:<8}  {:<8}  {:<10}  {:<10}  installed_path",
        "id", "scope", "version", "status", "updated"
    );
    for (id, entry, on) in rows {
        let status = if on { "enabled" } else { "installed" };
        let updated = entry.last_updated.format("%Y-%m-%d");
        println!(
            "{:<28}  {:<8}  {:<8}  {:<10}  {:<10}  {}",
            id,
            scope_label(entry.scope),
            entry.version,
            status,
            updated,
            entry.install_path.display()
        );
    }
    Ok(())
}

/// `reflect plugin info <id>` —— 单条详细(对齐 `reflect mcp show`)。
pub fn info(id_str: &str) -> anyhow::Result<()> {
    let (mgr, cfg) = load_manager_and_config()?;
    let id = parse_id(id_str)?;
    let Some(entry) = mgr.lookup_entry(&id) else {
        return Err(anyhow!("plugin '{id}' not installed"));
    };
    let enabled = cfg.plugins.is_enabled(id.as_str());
    let enabled_at = cfg
        .plugins
        .enabled_plugins
        .iter()
        .find(|p| *p == id.as_str())
        .cloned()
        .unwrap_or_default();
    println!("id            : {id}");
    println!("scope         : {}", scope_label(entry.scope));
    println!("version       : {}", entry.version);
    println!(
        "status        : {}",
        if enabled { "enabled" } else { "installed" }
    );
    println!("install_path  : {}", entry.install_path.display());
    println!(
        "installed_at  : {}",
        entry.installed_at.format("%Y-%m-%d %H:%M:%SZ")
    );
    println!(
        "last_updated  : {}",
        entry.last_updated.format("%Y-%m-%d %H:%M:%SZ")
    );
    if enabled {
        println!("enabled_in    : config.toml#plugins.enabled_plugins");
        println!("  └─ entry    : {enabled_at}");
    } else {
        println!("enabled_in    : (not in config.toml#plugins.enabled_plugins)");
    }
    Ok(())
}

/// `reflect plugin enable <id>` —— 写 ReflectConfig.plugins.enabled_plugins。
pub fn enable(id_str: &str) -> anyhow::Result<()> {
    let (mgr, mut cfg) = load_manager_and_config()?;
    let id = parse_id(id_str)?;
    // 校验 installed(否则 enable 一个不存在的 plugin 没意义)。
    if mgr.lookup_entry(&id).is_none() {
        return Err(anyhow!(
            "plugin '{id}' not installed; run `reflect plugin install <path>` first"
        ));
    }
    let key = id.to_string();
    if !cfg.plugins.enabled_plugins.iter().any(|p| p == &key) {
        cfg.plugins.enabled_plugins.push(key);
        save_config(&cfg)?;
    }
    println!("enabled {id} (added to config.toml#plugins.enabled_plugins)");
    println!("Restart `reflect exec` to load capabilities.");
    Ok(())
}

/// `reflect plugin disable <id>` —— 从 enabled_plugins 移除。
pub fn disable(id_str: &str) -> anyhow::Result<()> {
    let (_mgr, mut cfg) = load_manager_and_config()?;
    let id = parse_id(id_str)?;
    let key = id.to_string();
    let before = cfg.plugins.enabled_plugins.len();
    cfg.plugins.enabled_plugins.retain(|p| p != &key);
    if cfg.plugins.enabled_plugins.len() == before {
        // 没动 → 也提示一下,免得用户以为是 bug。
        println!("{id} was not in config.toml#plugins.enabled_plugins (no-op)");
        return Ok(());
    }
    save_config(&cfg)?;
    println!("disabled {id} (removed from config.toml#plugins.enabled_plugins)");
    println!("Restart `reflect exec` to unload capabilities.");
    Ok(())
}

/// `reflect plugin uninstall <id>` —— 双写:installed + enabled。
///
/// Managed scope 拒绝(由 `PluginManager::uninstall` 拦截)。
pub fn uninstall(id_str: &str, scope: PluginScope, yes: bool) -> anyhow::Result<()> {
    let root =
        default_plugins_root().ok_or_else(|| anyhow!("HOME unset; cannot locate plugins dir"))?;
    let mut mgr = PluginManager::load(&root)?;
    let id = parse_id(id_str)?;
    if !yes {
        eprint!("uninstall {id} ({} scope)? [y/N] ", scope_label(scope));
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return Ok(());
        }
    }
    mgr.uninstall(&id, scope)
        .with_context(|| format!("uninstall failed for {id}"))?;

    // 同时从 enabled_plugins 中清理。
    let mut cfg = load_default();
    let key = id.to_string();
    let before = cfg.plugins.enabled_plugins.len();
    cfg.plugins.enabled_plugins.retain(|p| p != &key);
    if cfg.plugins.enabled_plugins.len() != before {
        save_config(&cfg)?;
    }
    println!("uninstalled {id}");
    Ok(())
}

/// `reflect plugin show <id>` —— 打印 manifest 原文(toml 格式)。
pub fn show_manifest(id_str: &str) -> anyhow::Result<()> {
    let root =
        default_plugins_root().ok_or_else(|| anyhow!("HOME unset; cannot locate plugins dir"))?;
    let mgr = PluginManager::load(&root)?;
    let id = parse_id(id_str)?;
    let entry = mgr
        .lookup_entry(&id)
        .ok_or_else(|| anyhow!("plugin '{id}' not installed"))?;
    let manifest_path = entry.install_path.join("plugin.toml");
    if !manifest_path.exists() {
        return Err(anyhow!(
            "manifest missing at {}; plugin cache may be corrupted",
            manifest_path.display()
        ));
    }
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    println!("[{}]", manifest_path.display());
    print!("{text}");
    Ok(())
}

// ── marketplace 子命令(Phase D)────────────────────────

/// `reflect plugin marketplace ls` —— 列已知 marketplace。
fn format_marketplace_source(src: &reflect_plugin::manifest::MarketplaceSource) -> String {
    use reflect_plugin::manifest::MarketplaceSource;
    match src {
        MarketplaceSource::Github { repo, .. } => format!("github:{repo}"),
        MarketplaceSource::Git { url, .. } => format!("git:{url}"),
        MarketplaceSource::Url { url, .. } => format!("url:{url}"),
        MarketplaceSource::File { path } => format!("file:{}", path.display()),
        MarketplaceSource::Directory { path } => format!("directory:{}", path.display()),
    }
}

// ── 共享辅助函数 ────────────────────────────────────────────────────

/// 把字符串解析成 `PluginId`,错误转 anyhow。
///
/// 用 `PluginId::parse_user_input`(宽松)而不是 `PluginId::parse`,
/// 因为 CLI 允许保留 marketplace 名(`inline` / `builtin`)—— 这两个对
/// 应本地 install 与 builtin 插件,是合法的 plugin id 形态。
fn parse_id(s: &str) -> anyhow::Result<PluginId> {
    PluginId::parse_user_input(s).map_err(|e| anyhow!("invalid plugin id '{s}': {e}"))
}

/// 把 scope enum 格式化为 short label。
fn scope_label(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::Managed => "managed",
        PluginScope::User => "user",
        PluginScope::Project => "project",
        PluginScope::Local => "local",
    }
}

/// UTF-8 字符级截断到 `max_chars`,超出追加 `…`。与 mcp.rs 同款。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count >= max_chars.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
    }
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        out
    }
}

// ── 单元测试 ──────────────────────────────────────────────────────────
