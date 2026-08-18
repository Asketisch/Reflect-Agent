//! `reflect plugin marketplace ...` —— marketplace add / ls / remove / refresh。
//!
//! 拆自 `plugin.rs`,语义零变化。

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use reflect_plugin::manifest::{MarketplaceManifest, MarketplaceSource};
use reflect_plugin::marketplace::{MarketplaceFetchRouter, MarketplaceUpdater};
use reflect_plugin::{KnownMarketplace, KnownMarketplacesFile};

use super::{default_marketplaces_path, default_plugins_root, format_marketplace_source, truncate};

pub fn marketplace_ls() -> anyhow::Result<()> {
    let Some(path) = default_marketplaces_path() else {
        println!("(HOME unset; cannot resolve marketplaces.json path)");
        return Ok(());
    };
    let f = load_known_marketplaces(&path)?;
    if f.marketplaces.is_empty() {
        println!(
            "(no marketplaces configured; run `reflect plugin marketplace add <name> --from ...`)"
        );
        return Ok(());
    }
    println!("{:<20}  {:<12}  source", "name", "auto_update");
    let mut names: Vec<_> = f.marketplaces.keys().collect();
    names.sort();
    for name in names {
        let m = &f.marketplaces[name];
        let auto = if m.auto_update { "yes" } else { "no" };
        let src = format_marketplace_source(&m.source);
        println!("{:<20}  {:<12}  {}", name, auto, truncate(&src, 60));
    }
    println!();
    println!("Use `reflect plugin marketplace refresh [--name <name>]` to fetch updates.");
    Ok(())
}

/// `reflect plugin marketplace add <name> --from <kind> [flags]` —— 真 fetch(5 种 source 都接通)。
///
/// Phase D/E:
/// - `git` / `file` / `directory` 走 `MarketplaceFetchRouter.fetch`
/// - `url`(Phase E)走 `UrlFetcher`(reqwest GET)
/// - `github`(Phase E)走 `GithubFetcher`(转 `Git` 委托)
/// - 拉完读 `.claude-plugin/marketplace.json` 验证 manifest 合法
/// - 写 known_marketplaces.json
pub async fn marketplace_add(
    name: &str,
    kind: crate::MarketplaceSourceKind,
    repo: Option<&str>,
    url: Option<&str>,
    git_ref: Option<&str>,
    sha: Option<&str>,
    path: Option<&str>,
) -> anyhow::Result<()> {
    use crate::MarketplaceSourceKind as Kind;
    let Some(mp_path) = default_marketplaces_path() else {
        return Err(anyhow!("HOME unset; cannot resolve marketplaces.json path"));
    };
    let plugins_root =
        default_plugins_root().ok_or_else(|| anyhow!("HOME unset; cannot locate plugins dir"))?;

    // 1. 构造 MarketplaceSource(按 source kind 校验必需 flag)。
    let source = match kind {
        Kind::Github => match repo {
            Some(r) => MarketplaceSource::Github {
                repo: r.into(),
                r#ref: None,
                sha: None,
            },
            None => return Err(anyhow!("--from github requires --repo owner/name")),
        },
        Kind::Git => match url {
            Some(u) => MarketplaceSource::Git {
                url: u.into(),
                r#ref: git_ref.map(String::from),
                sha: sha.map(String::from),
                path: None,
            },
            None => return Err(anyhow!("--from git requires --url <git-url>")),
        },
        Kind::Url => match url {
            Some(u) => MarketplaceSource::Url {
                url: u.into(),
                headers: Default::default(),
            },
            None => return Err(anyhow!("--from url requires --url <marketplace-url>")),
        },
        Kind::File => match path {
            Some(p) => MarketplaceSource::File {
                path: PathBuf::from(p),
            },
            None => return Err(anyhow!("--from file requires --path <marketplace.json>")),
        },
        Kind::Directory => match path {
            Some(p) => MarketplaceSource::Directory {
                path: PathBuf::from(p),
            },
            None => {
                return Err(anyhow!(
                    "--from directory requires --path <marketplace-dir>"
                ));
            }
        },
    };

    // 2. marketplace 名字校验(保留名拒绝)。
    let name_parsed: reflect_plugin::MarketplaceName = reflect_plugin::MarketplaceName::parse(name)
        .map_err(|e| anyhow!("invalid marketplace name '{name}': {e}"))?;

    // 3. 真 fetch(异步)。Phase E 起所有 5 种 source 都真接通,无 stub。
    let default_install_location = plugins_root.join("marketplaces").join(name);
    let router = MarketplaceFetchRouter::new();
    let mkt_root = router
        .fetch(&source, &default_install_location)
        .await
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| format!("fetch marketplace '{name}' failed"))?;
    // Directory fetcher 不复制(直接返回 source.path),所以 install_location 应指 source.path。
    // Git / File / Url / Github 已把内容写到 default_install_location,直接用。
    let install_location = if matches!(source, MarketplaceSource::Directory { .. }) {
        mkt_root.clone()
    } else {
        default_install_location
    };

    // 4. 验证 manifest 存在且可解析(对 Directory 是 source.path,其他是 install_location)。
    let manifest_path = MarketplaceManifest::find_in_dir(&mkt_root).ok_or_else(|| {
        anyhow!(
            "marketplace manifest not found at {}/.claude-plugin/marketplace.json",
            mkt_root.display()
        )
    })?;
    let _manifest = MarketplaceManifest::from_json_path(&manifest_path)
        .with_context(|| format!("parse {}", manifest_path.display()))?;

    // 5. 写 known_marketplaces.json。
    let mut f = load_known_marketplaces(&mp_path)?;
    f.upsert(
        name_parsed,
        KnownMarketplace {
            source,
            install_location,
            last_updated: reflect_plugin::state_now(),
            auto_update: true,
        },
    );
    write_known_marketplaces(&mp_path, &f)?;

    println!(
        "added marketplace '{name}' (manifest at {})",
        manifest_path.display()
    );
    println!(
        "Use `reflect plugin install <plugin>@{name}` to install a plugin from this marketplace."
    );
    Ok(())
}

/// `reflect plugin marketplace remove <name>` —— 从 known_marketplaces.json 移除 + 真删 cache。
pub fn marketplace_remove(name: &str) -> anyhow::Result<()> {
    let Some(mp_path) = default_marketplaces_path() else {
        return Err(anyhow!("HOME unset; cannot resolve marketplaces.json path"));
    };
    let mut f = load_known_marketplaces(&mp_path)?;
    let name_parsed: reflect_plugin::MarketplaceName = reflect_plugin::MarketplaceName::parse(name)
        .map_err(|e| anyhow!("invalid marketplace name '{name}': {e}"))?;
    let removed = f.remove(&name_parsed);
    let Some(entry) = removed else {
        return Err(anyhow!("marketplace '{name}' not registered"));
    };
    write_known_marketplaces(&mp_path, &f)?;

    // 真删 cache(若存在)。
    let mut cache_msg = String::from("(no cache to purge)");
    if entry.install_location.exists() {
        match std::fs::remove_dir_all(&entry.install_location) {
            Ok(()) => {
                cache_msg = format!("(purged cache at {})", entry.install_location.display());
            }
            Err(e) => {
                cache_msg = format!(
                    "(WARN: cache at {} not purged: {})",
                    entry.install_location.display(),
                    e
                );
                tracing::warn!(
                    path = %entry.install_location.display(),
                    error = %e,
                    "marketplace_remove: cache purge failed"
                );
            }
        }
    }
    println!("removed marketplace '{name}' {cache_msg}");
    Ok(())
}

/// `reflect plugin marketplace refresh [--name <name>]` —— Phase E:手动触发刷新。
///
/// 不传 `--name` → 刷全部 `auto_update = true` 的 marketplace;
/// 传 `--name <x>` → 只刷 `<x>`(忽略 auto_update 标志)。
///
/// 失败的 marketplace 会 `WARN: ...` 打印但不影响其他;成功则更新 `last_updated`
/// 写回 `marketplaces.json`。
pub async fn marketplace_refresh(name: Option<&str>) -> anyhow::Result<()> {
    let Some(mp_path) = default_marketplaces_path() else {
        return Err(anyhow!("HOME unset; cannot resolve marketplaces.json path"));
    };
    let mut f = load_known_marketplaces(&mp_path)?;
    let updater = MarketplaceUpdater::new();

    match name {
        Some(n) => {
            let name_parsed: reflect_plugin::MarketplaceName =
                reflect_plugin::MarketplaceName::parse(n)
                    .map_err(|e| anyhow!("invalid marketplace name '{n}': {e}"))?;
            let entry = f
                .marketplaces
                .get(&name_parsed)
                .ok_or_else(|| anyhow!("marketplace '{n}' not registered"))?;
            let result = updater.refresh_one(&name_parsed, entry).await;
            match &result {
                Ok(()) => {
                    // 更新 last_updated 落盘。
                    if let Some(e_mut) = f.marketplaces.get_mut(&name_parsed) {
                        e_mut.last_updated = reflect_plugin::state_now();
                    }
                    write_known_marketplaces(&mp_path, &f)?;
                    println!("refreshed marketplace '{n}' OK");
                }
                Err(e) => {
                    eprintln!("WARN: refresh marketplace '{n}' failed: {e}");
                }
            }
            result.map_err(anyhow::Error::from)
        }
        None => {
            if f.marketplaces.is_empty() {
                println!("(no marketplaces configured)");
                return Ok(());
            }
            let outcomes = updater.refresh_all(&f).await;
            let mut any_failure = false;
            for (n, r) in outcomes {
                match r {
                    Ok(()) => {
                        if let Some(e_mut) = f.marketplaces.get_mut(&n) {
                            e_mut.last_updated = reflect_plugin::state_now();
                        }
                        println!("refreshed marketplace '{n}' OK");
                    }
                    Err(e) => {
                        eprintln!("WARN: refresh marketplace '{n}' failed: {e}");
                        any_failure = true;
                    }
                }
            }
            // 批量也写回(成功的 last_updated 推进)。
            write_known_marketplaces(&mp_path, &f)?;
            if any_failure {
                Err(anyhow!("one or more marketplaces failed to refresh"))
            } else {
                Ok(())
            }
        }
    }
}

pub(crate) fn load_known_marketplaces(path: &Path) -> anyhow::Result<KnownMarketplacesFile> {
    if !path.exists() {
        return Ok(KnownMarketplacesFile::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub(crate) fn write_known_marketplaces(
    path: &Path,
    f: &KnownMarketplacesFile,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(f)?;
    // 原子写:.tmp → rename。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
