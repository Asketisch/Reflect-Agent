//! `MarketplaceUpdater` —— 启动期/手动触发的 marketplace 增量刷新。
//!
//! 持有 `MarketplaceFetchRouter` 与 `KnownMarketplacesFile` 视图,
//! 提供:
//! - `refresh_one(name, known) -> Result<()>` —— 刷新单个 marketplace
//! - `refresh_all(file) -> Vec<(MarketplaceName, Result<()>)>` —— 批量刷新,
//!   返回每条结果(不短路,失败也继续)
use crate::errors::{PluginError, Result};
use crate::identifier::MarketplaceName;
use crate::state::{KnownMarketplace, KnownMarketplacesFile};

use super::fetcher::MarketplaceFetchRouter;

/// 单条 marketplace 的刷新结果 ——
/// `Err` 时只记录 `tracing::warn!`,不阻断 TUI / CLI 后续。
pub struct MarketplaceUpdater {
    router: MarketplaceFetchRouter,
}

impl MarketplaceUpdater {
    pub fn new() -> Self {
        Self {
            router: MarketplaceFetchRouter::new(),
        }
    }

    /// 刷新单个 marketplace:
    /// - 若 `known.install_location` 不存在 → 走 fetch(从 source 全量拉一次)
    /// - 若存在 → 走 update(增量,各 fetcher 自己的策略)
    ///
    /// 任何错误都 wrap 成 `PluginError::MarketplaceFetch`,便于上层 catch。
    pub async fn refresh_one(
        &self,
        name: &MarketplaceName,
        known: &KnownMarketplace,
    ) -> Result<()> {
        let dest = &known.install_location;
        if !dest.exists() {
            // 全量 fetch。
            self.router.fetch(&known.source, dest).await.map_err(|e| {
                PluginError::MarketplaceFetch {
                    kind: format!("refresh: {name}"),
                    stderr: format!("{e}"),
                }
            })?;
            tracing::info!(marketplace = %name, "refreshed (full fetch) marketplace");
        } else {
            // 增量 update。
            self.router.update(&known.source, dest).await.map_err(|e| {
                PluginError::MarketplaceFetch {
                    kind: format!("refresh: {name}"),
                    stderr: format!("{e}"),
                }
            })?;
            tracing::info!(marketplace = %name, "refreshed (update) marketplace");
        }
        Ok(())
    }

    /// 批量刷新 —— `known.auto_update = true` 的 marketplace 才会真正触发。
    /// 返回每条结果(name + Ok/Err),不短路(单个失败不影响其他)。
    pub async fn refresh_all(
        &self,
        known_file: &KnownMarketplacesFile,
    ) -> Vec<(MarketplaceName, Result<()>)> {
        // 收集需要刷新的 entries(按 name 字典序),便于 deterministic 顺序。
        let mut targets: Vec<(&MarketplaceName, &KnownMarketplace)> = known_file
            .marketplaces
            .iter()
            .filter(|(_, m)| m.auto_update)
            .collect();
        targets.sort_by(|a, b| a.0.cmp(b.0));

        let mut out = Vec::with_capacity(targets.len());
        for (name, m) in targets {
            let r = self.refresh_one(name, m).await;
            out.push((name.clone(), r));
        }
        out
    }
}

impl Default for MarketplaceUpdater {
    fn default() -> Self {
        Self::new()
    }
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Stdio;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::manifest::MarketplaceSource;

    /// 准备一个本地 bare 仓库 + 工作副本(commit 一个空文件)。
    /// 返回 (TempDir 维持存活, bare_path, work_dir, work_path)。
    async fn make_local_git_repo() -> (TempDir, PathBuf, TempDir, PathBuf) {
        let bare_dir = TempDir::new().unwrap();
        let bare_path = bare_dir.path().to_path_buf();
        let status = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&bare_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git init --bare failed");

        let work_dir = TempDir::new().unwrap();
        let work_path = work_dir.path().to_path_buf();
        let status = Command::new("git")
            .arg("clone")
            .arg(&bare_path)
            .arg(&work_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git clone failed");

        fs::write(work_path.join("README.md"), "# marketplace\n").unwrap();
        for args in [
            vec!["add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
            vec!["push", "origin", "master"],
        ] {
            let mut cmd = Command::new("git");
            cmd.current_dir(&work_path);
            for a in &args {
                cmd.arg(a);
            }
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let status = cmd.status().await.unwrap();
            assert!(status.success(), "git step {args:?} failed");
        }
        (bare_dir, bare_path, work_dir, work_path)
    }

    #[tokio::test]
    async fn refresh_one_git_full_fetch_when_dest_missing() {
        let (bare_keep, bare_path, _work_dir, _work_path) = make_local_git_repo().await;
        let cache_dir = TempDir::new().unwrap();
        let install_location = cache_dir.path().join("mkt");
        // install_location 不存在 → 走 fetch 全量。
        assert!(!install_location.exists());

        let name = MarketplaceName::parse("git-mkt").unwrap();
        let known = KnownMarketplace {
            source: MarketplaceSource::Git {
                url: bare_path.display().to_string(),
                r#ref: None,
                sha: None,
                path: None,
            },
            install_location: install_location.clone(),
            last_updated: crate::state_now(),
            auto_update: true,
        };

        let updater = MarketplaceUpdater::new();
        updater.refresh_one(&name, &known).await.unwrap();
        // 全量拉取后 install_location 存在。
        assert!(install_location.exists());
        assert!(install_location.join("README.md").exists());
        let _ = bare_keep;
    }

    #[tokio::test]
    async fn refresh_one_git_update_pulls_new_commits() {
        let (bare_keep, bare_path, _work_dir, work_path) = make_local_git_repo().await;
        let cache_dir = TempDir::new().unwrap();
        let install_location = cache_dir.path().join("mkt");

        let name = MarketplaceName::parse("git-mkt").unwrap();
        let source = MarketplaceSource::Git {
            url: bare_path.display().to_string(),
            r#ref: None,
            sha: None,
            path: None,
        };
        let known = KnownMarketplace {
            source: source.clone(),
            install_location: install_location.clone(),
            last_updated: crate::state_now(),
            auto_update: true,
        };

        // 1. 首次 refresh → 全量 fetch。
        let updater = MarketplaceUpdater::new();
        updater.refresh_one(&name, &known).await.unwrap();
        assert!(install_location.join("README.md").exists());
        assert!(!install_location.join("CHANGELOG.md").exists());

        // 2. 在 work dir push 新 commit。
        fs::write(work_path.join("CHANGELOG.md"), "# new\n").unwrap();
        for args in [
            vec!["add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "second",
            ],
            vec!["push", "origin", "master"],
        ] {
            let mut cmd = Command::new("git");
            cmd.current_dir(&work_path);
            for a in &args {
                cmd.arg(a);
            }
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let status = cmd.status().await.unwrap();
            assert!(status.success(), "git step {args:?} failed");
        }

        // 3. 再次 refresh → update(增量)→ 应拉到 CHANGELOG。
        updater.refresh_one(&name, &known).await.unwrap();
        assert!(install_location.join("CHANGELOG.md").exists());
        let _ = bare_keep;
    }

    #[tokio::test]
    async fn refresh_one_directory_is_noop_when_dest_exists() {
        // 准备 directory source + 同步 install_location = source.path
        let mkt_dir = TempDir::new().unwrap();
        fs::create_dir_all(mkt_dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            mkt_dir
                .path()
                .join(".claude-plugin")
                .join("marketplace.json"),
            r#"{"name":"d-mkt","owner":{"name":"X"},"plugins":[]}"#,
        )
        .unwrap();

        let name = MarketplaceName::parse("d-mkt").unwrap();
        let known = KnownMarketplace {
            source: MarketplaceSource::Directory {
                path: mkt_dir.path().to_path_buf(),
            },
            install_location: mkt_dir.path().to_path_buf(),
            last_updated: crate::state_now(),
            auto_update: true,
        };

        let updater = MarketplaceUpdater::new();
        // install_location = source.path 已存在 → update 走 no-op 路径,不应报错。
        updater.refresh_one(&name, &known).await.unwrap();
        assert!(
            mkt_dir
                .path()
                .join(".claude-plugin/marketplace.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn refresh_one_file_recopies_on_update() {
        // 1. 准备 source JSON。
        let src_dir = TempDir::new().unwrap();
        let src_json = src_dir.path().join("marketplace.json");
        fs::write(
            &src_json,
            r#"{"name":"f-mkt","owner":{"name":"X"},"plugins":[]}"#,
        )
        .unwrap();
        let cache_dir = TempDir::new().unwrap();
        let install_location = cache_dir.path().join("mkt");

        let name = MarketplaceName::parse("f-mkt").unwrap();
        let known = KnownMarketplace {
            source: MarketplaceSource::File {
                path: src_json.clone(),
            },
            install_location: install_location.clone(),
            last_updated: crate::state_now(),
            auto_update: true,
        };

        // 2. 首次 refresh → fetch。
        let updater = MarketplaceUpdater::new();
        updater.refresh_one(&name, &known).await.unwrap();
        let copied = install_location
            .join(".claude-plugin")
            .join("marketplace.json");
        assert!(copied.exists());

        // 3. 改 source + 再 refresh → update 应重新 copy。
        fs::write(
            &src_json,
            r#"{"name":"f-mkt","owner":{"name":"X"},"plugins":[{"name":"new","version":"1.0.0","source":{"source":"file","path":"./x"}}]}"#,
        )
        .unwrap();
        updater.refresh_one(&name, &known).await.unwrap();
        let body = fs::read_to_string(&copied).unwrap();
        assert!(body.contains("\"new\""), "got body: {body}");
    }

    #[tokio::test]
    async fn refresh_all_skips_disabled_auto_update() {
        // 已存在两个 marketplace entry,只有 auto_update=true 的那个被刷。
        let mkt_dir = TempDir::new().unwrap();
        fs::create_dir_all(mkt_dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            mkt_dir
                .path()
                .join(".claude-plugin")
                .join("marketplace.json"),
            r#"{"name":"a","owner":{"name":"X"},"plugins":[]}"#,
        )
        .unwrap();

        let mut file = KnownMarketplacesFile::new();
        let name_on = MarketplaceName::parse("on").unwrap();
        file.upsert(
            name_on.clone(),
            KnownMarketplace {
                source: MarketplaceSource::Directory {
                    path: mkt_dir.path().to_path_buf(),
                },
                install_location: mkt_dir.path().to_path_buf(),
                last_updated: crate::state_now(),
                auto_update: true,
            },
        );
        let name_off = MarketplaceName::parse("off").unwrap();
        file.upsert(
            name_off.clone(),
            KnownMarketplace {
                source: MarketplaceSource::Directory {
                    path: mkt_dir.path().to_path_buf(),
                },
                install_location: mkt_dir.path().to_path_buf(),
                last_updated: crate::state_now(),
                auto_update: false,
            },
        );

        let updater = MarketplaceUpdater::new();
        let out = updater.refresh_all(&file).await;
        // 只有 "on" 被刷新。
        assert_eq!(out.len(), 1, "got {out:?}");
        assert_eq!(out[0].0, name_on);
    }

    #[tokio::test]
    async fn refresh_all_returns_vec_with_one_entry_per_marketplace() {
        // 两个 auto_update=true 的 marketplace 都返回。
        let mkt_dir = TempDir::new().unwrap();
        fs::create_dir_all(mkt_dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            mkt_dir
                .path()
                .join(".claude-plugin")
                .join("marketplace.json"),
            r#"{"name":"x","owner":{"name":"X"},"plugins":[]}"#,
        )
        .unwrap();

        let mut file = KnownMarketplacesFile::new();
        for n in ["alpha", "beta"] {
            file.upsert(
                MarketplaceName::parse(n).unwrap(),
                KnownMarketplace {
                    source: MarketplaceSource::Directory {
                        path: mkt_dir.path().to_path_buf(),
                    },
                    install_location: mkt_dir.path().to_path_buf(),
                    last_updated: crate::state_now(),
                    auto_update: true,
                },
            );
        }

        let updater = MarketplaceUpdater::new();
        let out = updater.refresh_all(&file).await;
        assert_eq!(out.len(), 2);
        // 按名字典序:alpha, beta。
        assert_eq!(out[0].0.as_str(), "alpha");
        assert_eq!(out[1].0.as_str(), "beta");
    }
}
