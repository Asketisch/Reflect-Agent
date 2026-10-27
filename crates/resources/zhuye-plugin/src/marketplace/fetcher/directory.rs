//! `DirectoryFetcher` —— 0 IO 验证,直接返回 source path。

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::MarketplaceFetcher;
use crate::errors::{PluginError, Result};
use crate::manifest::MarketplaceSource;

/// 0 IO 验证 —— 不复制,直接返回 source path。
///
/// 调用方需要后续 `MarketplaceManifest::find_in_dir(source.path)` 读 manifest。
pub struct DirectoryFetcher;

#[async_trait]
impl MarketplaceFetcher for DirectoryFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let src = match source {
            MarketplaceSource::Directory { path } => path.clone(),
            _ => unreachable!("router 必须按 variant 分派"),
        };
        // dest 参数在 Directory 语义下无意义(不复制),仍接收以保持 trait 形态。
        let _ = dest;
        if !src.is_dir() {
            return Err(PluginError::MarketplaceFetch {
                kind: format!("{source:?}"),
                stderr: format!("marketplace 目录不存在: {}", src.display()),
            });
        }
        // 预先验证 `.claude-plugin/marketplace.json` 存在 —— 早 fail,
        // 让调用方不必等 `find_in_dir` 才发现。
        let manifest = src.join(".claude-plugin").join("marketplace.json");
        if !manifest.is_file() {
            return Err(PluginError::MarketplaceManifestNotFound(
                manifest.display().to_string(),
            ));
        }
        Ok(src)
    }

    /// 增量刷新:Directory 0 IO 校验。
    /// 不主动 push / 改用户文件 —— refresh 时 no-op,但要确认 manifest 仍在。
    async fn update(&self, source: &MarketplaceSource, _dest: &Path) -> Result<()> {
        let src = match source {
            MarketplaceSource::Directory { path } => path.clone(),
            _ => unreachable!("router 必须按 variant 分派"),
        };
        let manifest = src.join(".claude-plugin").join("marketplace.json");
        if !manifest.is_file() {
            return Err(PluginError::MarketplaceManifestNotFound(
                manifest.display().to_string(),
            ));
        }
        // 记录到 tracing,便于用户 debug;不报错(本地 source 不主动 fetch)。
        tracing::debug!(path = %src.display(), "directory marketplace refresh: no-op");
        Ok(())
    }
}
