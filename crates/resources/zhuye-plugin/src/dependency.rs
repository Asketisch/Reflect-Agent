//! 依赖解析与错误聚合 —— Phase F。
//!
//! `DepResolver::resolve` 走 DFS 装一个 plugin 及其 closure 的所有依赖,
//! 解决跨 marketplace 解析(`PluginDependency::Qualified`)、检测环、
//! 失败聚合到 `InstallReport`,**不**中断整体流程。
//!
//! 设计要点:
//! - **DFS + visited set** —— 环检测用 visited set;遇到已访问 → 报 `DependencyCycle`。
//! - **后序 install** —— deps 先装,root 最后(避免 root 装成功但 dep 失败导致
//!   状态不一致)。实现上:递归到 leaf 才开始 `install_local` / `install_from_marketplace`,
//!   parent 在所有 dep 走完后再 install。
//! - **跨 marketplace** —— `PluginDependency::Qualified { name, marketplace }` 在
//!   `known.marketplaces[&marketplace]` 中找 cache_root;未注册 → `DependencyCrossMarketplace`。
//! - **不阻断** —— 单个 dep 失败只 push 到 `report.failed`,继续走其他 branch;
//!   整树走完才返回 `InstallReport`。
//! - **idempotent** —— 同 `(name, marketplace)` 已装过则 skip;同一 dep 多次出现
//!   也只装一次。

use std::collections::HashSet;
use std::path::Path;

use crate::errors::{PluginError, Result};
use crate::identifier::{MarketplaceName, PluginId};
use crate::manager::{InstallReport, PluginInstallFailure, PluginManager};
use crate::manifest::{MarketplaceManifest, PluginDependency, PluginMarketplaceEntry};
use crate::state::{KnownMarketplacesFile, PluginScope};

/// 已 resolve 的依赖节点 —— 包含 (plugin name, marketplace, cache root)。
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub name: String,
    pub marketplace: MarketplaceName,
    pub cache_root: std::path::PathBuf,
}

impl ResolvedDep {
    pub fn plugin_id(&self) -> Result<PluginId> {
        PluginId::new(&self.name, self.marketplace.as_str())
    }
}

/// DepResolver —— 持有 `KnownMarketplacesFile` 视图 + `PluginManager` 引用,
/// 走 dep closure,聚合到 `InstallReport`。
pub struct DepResolver<'a> {
    pub known: &'a KnownMarketplacesFile,
    pub mgr: &'a mut PluginManager,
    /// 检测环;每次 `resolve_node` 把当前 stack push 进来,出栈时 pop。
    visiting: HashSet<(String, String)>,
    /// 已 install 完成;重复 dep 引用 → skip(不动 installed,也不重 install)。
    done: HashSet<(String, String)>,
}

impl<'a> DepResolver<'a> {
    pub fn new(known: &'a KnownMarketplacesFile, mgr: &'a mut PluginManager) -> Self {
        Self {
            known,
            mgr,
            visiting: HashSet::new(),
            done: HashSet::new(),
        }
    }

    /// 入口:解析 root 节点 + 它的 deps,装完返回 `InstallReport`。
    pub fn resolve(&mut self, root: ResolvedDep, scope: PluginScope) -> InstallReport {
        let mut report = InstallReport::default();
        self.resolve_node(&root, scope, &mut report);
        report
    }

    /// DFS 递归:装 root 的所有 deps,然后装 root 本身。
    fn resolve_node(&mut self, node: &ResolvedDep, scope: PluginScope, report: &mut InstallReport) {
        let key = (node.name.clone(), node.marketplace.to_string());
        if self.done.contains(&key) {
            return; // 已装过 — idempotent
        }
        if self.visiting.contains(&key) {
            // 环:把当前 visiting + 自己 dump 出来,便于 trace。
            let mut cycle: Vec<String> = self
                .visiting
                .iter()
                .map(|(n, m)| format!("{n}@{m}"))
                .collect();
            cycle.push(format!("{}@{}", node.name, node.marketplace));
            report.failed.push(PluginInstallFailure {
                target: format!("{}@{}", node.name, node.marketplace),
                error: PluginError::DependencyCycle { cycle },
            });
            return;
        }
        self.visiting.insert(key.clone());

        // 读 marketplace manifest 找 entry。
        let entry = match read_entry(&node.cache_root, &node.name) {
            Ok(e) => e,
            Err(e) => {
                report.failed.push(PluginInstallFailure {
                    target: format!("{}@{}", node.name, node.marketplace),
                    error: e,
                });
                self.visiting.remove(&key);
                return;
            }
        };

        // 递归解析 deps(后序:deps 先)。
        for dep in &entry.manifest.dependencies {
            match self.lookup_dep(dep, node) {
                Ok(child) => {
                    self.resolve_node(&child, scope, report);
                }
                Err(e) => {
                    report.failed.push(PluginInstallFailure {
                        target: dep.to_string(),
                        error: e,
                    });
                }
            }
        }

        // 装自己。
        let local_path = match resolve_local_path(&node.cache_root, &entry) {
            Ok(p) => p,
            Err(e) => {
                report.failed.push(PluginInstallFailure {
                    target: format!("{}@{}", node.name, node.marketplace),
                    error: e,
                });
                self.visiting.remove(&key);
                return;
            }
        };
        match self
            .mgr
            .install_local(&local_path, &node.marketplace, scope)
        {
            Ok((id, entry)) => {
                tracing::info!(
                    plugin = %id,
                    "DepResolver: installed"
                );
                report.installed.push((id, entry));
                self.done.insert(key.clone());
            }
            Err(e) => {
                // 已装过(AlreadyInstalled)→ 算 idempotent 成功,记 done。
                if matches!(e, PluginError::AlreadyInstalled(_)) {
                    self.done.insert(key.clone());
                } else {
                    report.failed.push(PluginInstallFailure {
                        target: format!("{}@{}", node.name, node.marketplace),
                        error: e,
                    });
                }
            }
        }
        self.visiting.remove(&key);
    }

    /// 把 `PluginDependency` 解析成 `ResolvedDep`:
    /// - `Bare(name)` → 在当前 marketplace manifest 内找
    /// - `Qualified { name, marketplace }` → 在 `known.marketplaces[&m]` 找 cache_root
    fn lookup_dep(&self, dep: &PluginDependency, parent: &ResolvedDep) -> Result<ResolvedDep> {
        match dep {
            PluginDependency::Bare(name) => {
                // 在 parent marketplace manifest 内找(share cache_root)。
                Ok(ResolvedDep {
                    name: name.clone(),
                    marketplace: parent.marketplace.clone(),
                    cache_root: parent.cache_root.clone(),
                })
            }
            PluginDependency::Qualified { name, marketplace } => {
                // 跨 marketplace 解析。
                let mkt = MarketplaceName::parse(marketplace).map_err(|e| {
                    PluginError::InvalidMarketplaceName(format!("{marketplace}: {e}"))
                })?;
                let known_entry = self.known.marketplaces.get(&mkt).ok_or_else(|| {
                    PluginError::DependencyCrossMarketplace {
                        dep: format!("{name}@{marketplace}"),
                        marketplace: mkt.to_string(),
                    }
                })?;
                Ok(ResolvedDep {
                    name: name.clone(),
                    marketplace: mkt,
                    cache_root: known_entry.install_location.clone(),
                })
            }
        }
    }
}

/// 读 marketplace manifest,找到 `name == plugin_name` 的 entry。
fn read_entry(cache_root: &Path, plugin_name: &str) -> Result<PluginMarketplaceEntry> {
    let manifest_path = MarketplaceManifest::find_in_dir(cache_root).ok_or_else(|| {
        PluginError::MarketplaceManifestNotFound(format!(
            "{}/.claude-plugin/marketplace.json",
            cache_root.display()
        ))
    })?;
    let manifest = MarketplaceManifest::from_json_path(&manifest_path)?;
    manifest
        .plugins
        .into_iter()
        .find(|e| e.manifest.name == plugin_name)
        .ok_or_else(|| {
            PluginError::NotInstalled(format!(
                "{plugin_name} (not in marketplace manifest at {})",
                cache_root.display()
            ))
        })
}

/// 把 entry 解析到本地 plugin 目录路径(支持 File / Directory source)。
/// 其他 source(Git/Github/Url)→ 报错(应该已 fetch 完毕由 Directory 走)。
fn resolve_local_path(
    cache_root: &Path,
    entry: &PluginMarketplaceEntry,
) -> Result<std::path::PathBuf> {
    let resolved = entry.resolve_plugin_source(cache_root);
    match &resolved {
        crate::manifest::MarketplaceSource::File { path }
        | crate::manifest::MarketplaceSource::Directory { path } => {
            if !path.exists() {
                return Err(PluginError::StateIo {
                    path: path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "marketplace plugin source path missing",
                    ),
                });
            }
            Ok(path.clone())
        }
        other => Err(PluginError::MarketplaceFetch {
            kind: format!("{other:?}"),
            stderr: format!(
                "marketplace entry 的 source {other:?} 需先单独 fetch(Phase D 仅支持 File / Directory)"
            ),
        }),
    }
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::PluginManager;
    use crate::state::{KnownMarketplace, PluginScope};
    use crate::{MarketplaceName, state_now};
    use std::fs;
    use tempfile::TempDir;

    /// 构造一个本地 marketplace 根 + 1 个 plugin entry(默认无 deps)。
    /// 返回 (cache_root, plugin_name)。
    fn make_marketplace_with_plugin(
        parent: &Path,
        mkt_name: &str,
        plugin_name: &str,
        plugin_version: &str,
        deps: &[&str],
    ) -> (std::path::PathBuf, String) {
        let mkt_root = parent.join(mkt_name);
        fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
        let plugin_dir = mkt_root.join("plugins").join(plugin_name);
        fs::create_dir_all(&plugin_dir).unwrap();
        // 写 plugin.toml 清单文件
        let mut toml = format!("name = \"{plugin_name}\"\nversion = \"{plugin_version}\"\n");
        if !deps.is_empty() {
            toml.push_str("dependencies = [\n");
            for d in deps {
                toml.push_str(&format!("  \"{d}\",\n"));
            }
            toml.push_str("]\n");
        }
        fs::write(plugin_dir.join("plugin.toml"), toml).unwrap();

        // 写 marketplace.json(每个 plugin 一条 entry,deps 在 manifest.dependencies)
        let deps_json: String = if deps.is_empty() {
            "".to_string()
        } else {
            let arr = deps
                .iter()
                .map(|d| format!("\"{d}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!(", \"dependencies\": [{arr}]")
        };
        let json = format!(
            r#"{{
                "name": "{mkt_name}",
                "owner": {{ "name": "Test" }},
                "plugins": [
                    {{
                        "name": "{plugin_name}",
                        "version": "{plugin_version}",
                        "source": {{ "source": "directory", "path": "./plugins/{plugin_name}" }}{deps_json}
                    }}
                ]
            }}"#
        );
        fs::write(
            mkt_root.join(".claude-plugin").join("marketplace.json"),
            json,
        )
        .unwrap();
        (mkt_root, plugin_name.to_string())
    }

    fn build_known(
        marketplaces: &[(MarketplaceName, std::path::PathBuf)],
    ) -> KnownMarketplacesFile {
        let mut f = KnownMarketplacesFile::new();
        for (n, p) in marketplaces {
            f.upsert(
                n.clone(),
                KnownMarketplace {
                    source: crate::manifest::MarketplaceSource::Directory { path: p.clone() },
                    install_location: p.clone(),
                    last_updated: state_now(),
                    auto_update: true,
                },
            );
        }
        f
    }

    #[test]
    fn resolve_installs_root_with_no_deps() {
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        let (m1_root, _) = make_marketplace_with_plugin(dir.path(), "m1", "p1", "1.0.0", &[]);
        let m1_name = MarketplaceName::parse("m1").unwrap();
        let known = build_known(&[(m1_name.clone(), m1_root.clone())]);

        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "p1".into(),
            marketplace: m1_name,
            cache_root: m1_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        assert!(report.is_success(), "report: {report:?}");
        assert_eq!(report.installed.len(), 1);
        assert!(report.installed[0].0.as_str().starts_with("p1@"));
    }

    #[test]
    fn resolve_installs_dep_before_root() {
        // 市场 m1:插件 p1(无依赖)
        // 市场 m1:插件 p2 → 依赖 p1
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();

        // 构造 p1 + p2 在同一 marketplace,p2 依赖 p1。
        let m1_root = dir.path().join("m1");
        fs::create_dir_all(m1_root.join(".claude-plugin")).unwrap();
        // p1
        let p1_dir = m1_root.join("plugins").join("p1");
        fs::create_dir_all(&p1_dir).unwrap();
        fs::write(
            p1_dir.join("plugin.toml"),
            "name = \"p1\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        // p2 依赖 p1
        let p2_dir = m1_root.join("plugins").join("p2");
        fs::create_dir_all(&p2_dir).unwrap();
        fs::write(
            p2_dir.join("plugin.toml"),
            "name = \"p2\"\nversion = \"1.0.0\"\ndependencies = [\"p1\"]\n",
        )
        .unwrap();
        // 写 marketplace.json
        fs::write(
            m1_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m1",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "p1", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p1" } },
                    { "name": "p2", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p2" },
                      "dependencies": ["p1"] }
                ]
            }"#,
        )
        .unwrap();
        let m1_name = MarketplaceName::parse("m1").unwrap();
        let known = build_known(&[(m1_name.clone(), m1_root.clone())]);

        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "p2".into(),
            marketplace: m1_name,
            cache_root: m1_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        assert!(report.is_success(), "report: {report:?}");
        assert_eq!(report.installed.len(), 2);
        // p1 应在 p2 之前装(后序)。
        assert!(report.installed[0].0.as_str().starts_with("p1@"));
        assert!(report.installed[1].0.as_str().starts_with("p2@"));
    }

    #[test]
    fn resolve_detects_cycle() {
        // p1 依赖 p2,p2 依赖 p1 → 环。
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        let m1_root = dir.path().join("m1");
        fs::create_dir_all(m1_root.join(".claude-plugin")).unwrap();
        for p in &["p1", "p2"] {
            let d = m1_root.join("plugins").join(p);
            fs::create_dir_all(&d).unwrap();
        }
        fs::write(
            m1_root.join("plugins").join("p1").join("plugin.toml"),
            "name = \"p1\"\nversion = \"1.0.0\"\ndependencies = [\"p2\"]\n",
        )
        .unwrap();
        fs::write(
            m1_root.join("plugins").join("p2").join("plugin.toml"),
            "name = \"p2\"\nversion = \"1.0.0\"\ndependencies = [\"p1\"]\n",
        )
        .unwrap();
        fs::write(
            m1_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m1",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "p1", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p1" },
                      "dependencies": ["p2"] },
                    { "name": "p2", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p2" },
                      "dependencies": ["p1"] }
                ]
            }"#,
        )
        .unwrap();
        let m1_name = MarketplaceName::parse("m1").unwrap();
        let known = build_known(&[(m1_name.clone(), m1_root.clone())]);
        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "p1".into(),
            marketplace: m1_name,
            cache_root: m1_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        // 应有 DependencyCycle 失败。
        let cycle_failures: Vec<_> = report
            .failed
            .iter()
            .filter(|f| matches!(f.error, PluginError::DependencyCycle { .. }))
            .collect();
        assert!(!cycle_failures.is_empty(), "expected cycle, got {report:?}");
    }

    #[test]
    fn resolve_cross_marketplace_via_qualified() {
        // 市场 m1:插件 p1(无依赖)
        // 市场 m2:插件 p2 → 依赖 p1@m1(限定市场)
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        // m1
        let m1_root = dir.path().join("m1");
        fs::create_dir_all(m1_root.join(".claude-plugin")).unwrap();
        let p1_dir = m1_root.join("plugins").join("p1");
        fs::create_dir_all(&p1_dir).unwrap();
        fs::write(
            p1_dir.join("plugin.toml"),
            "name = \"p1\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            m1_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m1",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "p1", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p1" } }
                ]
            }"#,
        )
        .unwrap();
        // m2
        let m2_root = dir.path().join("m2");
        fs::create_dir_all(m2_root.join(".claude-plugin")).unwrap();
        let p2_dir = m2_root.join("plugins").join("p2");
        fs::create_dir_all(&p2_dir).unwrap();
        fs::write(
            p2_dir.join("plugin.toml"),
            "name = \"p2\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            m2_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m2",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "p2", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p2" },
                      "dependencies": ["p1@m1"] }
                ]
            }"#,
        )
        .unwrap();
        let m1_name = MarketplaceName::parse("m1").unwrap();
        let m2_name = MarketplaceName::parse("m2").unwrap();
        let known = build_known(&[
            (m1_name, m1_root.clone()),
            (m2_name.clone(), m2_root.clone()),
        ]);

        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "p2".into(),
            marketplace: m2_name,
            cache_root: m2_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        assert!(report.is_success(), "report: {report:?}");
        // 2 个 plugin:p1@m1, p2@m2
        assert_eq!(report.installed.len(), 2);
    }

    #[test]
    fn resolve_cross_marketplace_fails_if_target_marketplace_not_registered() {
        // p1@m1 引用,但 known 只有 m2 → DependencyCrossMarketplace。
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        let m2_root = dir.path().join("m2");
        fs::create_dir_all(m2_root.join(".claude-plugin")).unwrap();
        let p2_dir = m2_root.join("plugins").join("p2");
        fs::create_dir_all(&p2_dir).unwrap();
        fs::write(
            p2_dir.join("plugin.toml"),
            "name = \"p2\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            m2_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m2",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "p2", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p2" },
                      "dependencies": ["p1@m1"] }
                ]
            }"#,
        )
        .unwrap();
        // known 只有 m2,没 m1
        let m2_name = MarketplaceName::parse("m2").unwrap();
        let known = build_known(&[(m2_name.clone(), m2_root.clone())]);

        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "p2".into(),
            marketplace: m2_name,
            cache_root: m2_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        let cross_failures: Vec<_> = report
            .failed
            .iter()
            .filter(|f| matches!(f.error, PluginError::DependencyCrossMarketplace { .. }))
            .collect();
        assert!(
            !cross_failures.is_empty(),
            "expected DependencyCrossMarketplace, got {report:?}"
        );
    }

    #[test]
    fn resolve_dedupes_repeated_dep() {
        // p1 依赖 x; p2 依赖 x; p_root 依赖 p1 + p2 → x 只装一次。
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        let m1_root = dir.path().join("m1");
        fs::create_dir_all(m1_root.join(".claude-plugin")).unwrap();
        for p in &["x", "p1", "p2", "root"] {
            let d = m1_root.join("plugins").join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("plugin.toml"),
                format!("name = \"{p}\"\nversion = \"1.0.0\"\n"),
            )
            .unwrap();
        }
        // x:无依赖
        // p1:依赖 = [x]
        fs::write(
            m1_root.join("plugins").join("p1").join("plugin.toml"),
            "name = \"p1\"\nversion = \"1.0.0\"\ndependencies = [\"x\"]\n",
        )
        .unwrap();
        // p2:依赖 = [x]
        fs::write(
            m1_root.join("plugins").join("p2").join("plugin.toml"),
            "name = \"p2\"\nversion = \"1.0.0\"\ndependencies = [\"x\"]\n",
        )
        .unwrap();
        // root:依赖 = [p1, p2]
        fs::write(
            m1_root.join("plugins").join("root").join("plugin.toml"),
            "name = \"root\"\nversion = \"1.0.0\"\ndependencies = [\"p1\", \"p2\"]\n",
        )
        .unwrap();
        fs::write(
            m1_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m1",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "x", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/x" } },
                    { "name": "p1", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p1" },
                      "dependencies": ["x"] },
                    { "name": "p2", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/p2" },
                      "dependencies": ["x"] },
                    { "name": "root", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/root" },
                      "dependencies": ["p1", "p2"] }
                ]
            }"#,
        )
        .unwrap();
        let m1_name = MarketplaceName::parse("m1").unwrap();
        let known = build_known(&[(m1_name.clone(), m1_root.clone())]);
        let mut mgr = PluginManager::new(&plugins_root);
        let root_node = ResolvedDep {
            name: "root".into(),
            marketplace: m1_name,
            cache_root: m1_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root_node, PluginScope::User);
        assert!(report.is_success(), "report: {report:?}");
        // x, p1, p2, root = 4 个,x 只装一次。
        assert_eq!(report.installed.len(), 4, "report: {report:?}");
    }

    #[test]
    fn resolve_root_failure_aggregates_dep_already_installed() {
        // x: ok
        // broken: 不存在的 plugin.toml(空目录 + manifest 但 source 指向 missing path)
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        fs::create_dir_all(&plugins_root).unwrap();
        let m1_root = dir.path().join("m1");
        fs::create_dir_all(m1_root.join(".claude-plugin")).unwrap();
        let x_dir = m1_root.join("plugins").join("x");
        fs::create_dir_all(&x_dir).unwrap();
        fs::write(
            x_dir.join("plugin.toml"),
            "name = \"x\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        // broken: plugin.toml 在但源文件 missing path(走 directory source → 目录不存在)
        let broken_dir = m1_root.join("plugins").join("broken");
        fs::create_dir_all(&broken_dir).unwrap();
        fs::write(
            broken_dir.join("plugin.toml"),
            "name = \"broken\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            m1_root.join(".claude-plugin").join("marketplace.json"),
            r#"{
                "name": "m1",
                "owner": { "name": "X" },
                "plugins": [
                    { "name": "x", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/x" } },
                    { "name": "broken", "version": "1.0.0",
                      "source": { "source": "directory", "path": "./plugins/broken" } }
                ]
            }"#,
        )
        .unwrap();
        // 把 broken 的 plugin source path 删了 → install_local 找不到 manifest
        fs::remove_dir_all(&broken_dir).unwrap();

        let m1_name = MarketplaceName::parse("m1").unwrap();
        let known = build_known(&[(m1_name.clone(), m1_root.clone())]);
        let mut mgr = PluginManager::new(&plugins_root);
        let root = ResolvedDep {
            name: "broken".into(),
            marketplace: m1_name,
            cache_root: m1_root,
        };
        let mut resolver = DepResolver::new(&known, &mut mgr);
        let report = resolver.resolve(root, PluginScope::User);
        // 失败聚合,broken 应在 failed 里。
        assert!(!report.is_success(), "expected failure, got {report:?}");
        assert!(!report.failed.is_empty());
    }
}
