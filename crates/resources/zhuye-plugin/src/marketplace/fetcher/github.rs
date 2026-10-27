//! `GithubFetcher` —— Phase E:`owner/repo` shorthand 委托 `GitFetcher`。

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::MarketplaceFetcher;
use super::git::GitFetcher;
use crate::errors::Result;
use crate::manifest::MarketplaceSource;

/// Phase E:`owner/repo` shorthand → 委托 `GitFetcher`,走 `https://github.com/<repo>.git`。
///
/// `ref` / `sha` 直接透传到 `Git` 字段,获得 `git clone --branch` / `git checkout` 全能力。
pub struct GithubFetcher;

impl GithubFetcher {
    /// 把 `Github` 转换成等价的 `Git` source。
    /// 注意:`owner/repo` 必须不含 `/` 之外的路径段,否则 Git 路径会拼错。
    pub fn to_git(source: &MarketplaceSource) -> Result<MarketplaceSource> {
        match source {
            MarketplaceSource::Github { repo, r#ref, sha } => Ok(MarketplaceSource::Git {
                url: format!("https://github.com/{repo}.git"),
                r#ref: r#ref.clone(),
                sha: sha.clone(),
                path: None,
            }),
            _ => unreachable!("router 必须按 variant 分派"),
        }
    }
}

#[async_trait]
impl MarketplaceFetcher for GithubFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let git_source = Self::to_git(source)?;
        GitFetcher.fetch(&git_source, dest).await
    }

    /// 增量刷新:与 GitFetcher 一致(`git pull --ff-only`)。
    async fn update(&self, source: &MarketplaceSource, dest: &Path) -> Result<()> {
        let git_source = Self::to_git(source)?;
        GitFetcher.update(&git_source, dest).await
    }
}
