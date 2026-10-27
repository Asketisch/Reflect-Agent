//! `MarketplaceFetcher` trait + 5 个实现(Git / File / Directory / Url / Github)。
//!
//! 路由分发规则(由 `MarketplaceFetchRouter::fetch` 统一负责):
//! - `MarketplaceSource::Git { .. }`        → `GitFetcher`
//! - `MarketplaceSource::File { .. }`       → `FileFetcher`
//! - `MarketplaceSource::Directory { .. }`  → `DirectoryFetcher`
//! - `MarketplaceSource::Url { .. }`        → `UrlFetcher`(Phase E:reqwest GET)
//! - `MarketplaceSource::Github { .. }`     → `GithubFetcher`(Phase E:转 `Git` 委托)
//!
//! 另:每个 fetcher 实现 `update`,在 `MarketplaceUpdater::refresh_*` 中复用,
//! 由 `MarketplaceFetcher::update` 统一签名,默认 `Err`。
//!
//! Git 实现要求运行环境有 `git`(2.x+,支持 `--filter=blob:none`)。

mod directory;
mod file;
mod git;
mod github;
mod url;
mod util;

#[cfg(test)]
mod tests;

pub use directory::DirectoryFetcher;
pub use file::FileFetcher;
pub use git::GitFetcher;
pub use github::GithubFetcher;
pub use url::UrlFetcher;

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::errors::{PluginError, Result};
use crate::manifest::MarketplaceSource;

/// 把 marketplace 物化到 `dest` 的统一抽象。
///
/// 返回值是后续读 `.claude-plugin/marketplace.json` 的根目录:
/// - `GitFetcher` / `FileFetcher` / `UrlFetcher`:返回 `dest` 本地路径
/// - `DirectoryFetcher`:返回 `source.path`(0 IO,直接用源)
/// - `GithubFetcher`:委托 `GitFetcher`,返回 `dest` 本地路径
#[async_trait]
pub trait MarketplaceFetcher: Send + Sync {
    /// 首次拉取:把 `source` 物化到 `dest`,返回 marketplace 根路径。
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf>;

    /// 增量刷新:假定 `dest` 已存在(由 `fetch` 写入),在此基础上更新。
    /// 默认实现:未特化的 fetcher 报 `MarketplaceFetch` error。
    async fn update(&self, _source: &MarketplaceSource, _dest: &Path) -> Result<()> {
        Err(PluginError::MarketplaceFetch {
            kind: "unsupported".into(),
            stderr: "this fetcher does not implement update".into(),
        })
    }
}

/// 路由 —— 按 `MarketplaceSource` variant 选具体 fetcher。
///
/// v0 简化成直接 `enum dispatch`(无虚函数表)。
pub struct MarketplaceFetchRouter {
    git: GitFetcher,
    file: FileFetcher,
    directory: DirectoryFetcher,
    url: UrlFetcher,
    github: GithubFetcher,
}

impl MarketplaceFetchRouter {
    pub fn new() -> Self {
        Self {
            git: GitFetcher,
            file: FileFetcher,
            directory: DirectoryFetcher,
            url: UrlFetcher,
            github: GithubFetcher,
        }
    }

    /// 派发 fetch —— 终态返回 marketplace 根路径(供 `find_in_dir`)。
    ///
    /// Phase E 起所有 5 种 source 都真接通,无 stub。
    pub async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        match source {
            MarketplaceSource::Git { .. } => self.git.fetch(source, dest).await,
            MarketplaceSource::File { .. } => self.file.fetch(source, dest).await,
            MarketplaceSource::Directory { .. } => self.directory.fetch(source, dest).await,
            MarketplaceSource::Url { .. } => self.url.fetch(source, dest).await,
            MarketplaceSource::Github { .. } => self.github.fetch(source, dest).await,
        }
    }

    /// 派发 update —— 假定 dest 已存在(由 fetch 写入)。
    pub async fn update(&self, source: &MarketplaceSource, dest: &Path) -> Result<()> {
        match source {
            MarketplaceSource::Git { .. } => self.git.update(source, dest).await,
            MarketplaceSource::File { .. } => self.file.update(source, dest).await,
            MarketplaceSource::Directory { .. } => self.directory.update(source, dest).await,
            MarketplaceSource::Url { .. } => self.url.update(source, dest).await,
            MarketplaceSource::Github { .. } => self.github.update(source, dest).await,
        }
    }
}

impl Default for MarketplaceFetchRouter {
    fn default() -> Self {
        Self::new()
    }
}
