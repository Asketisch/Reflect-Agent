//! `FileFetcher` —— 把单文件 `marketplace.json` 复制到 dest。

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::MarketplaceFetcher;
use crate::errors::{PluginError, Result};
use crate::manifest::MarketplaceSource;

/// 把单文件 `marketplace.json` 复制到 `<dest>/.claude-plugin/marketplace.json`。
///
/// 假设 source 本身就是 marketplace.json,不是 plugin.json。
/// v0 不做内容嗅探 —— 调用方要保证 `File { path }` 指向 marketplace.json。
pub struct FileFetcher;

#[async_trait]
impl MarketplaceFetcher for FileFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let path = match source {
            MarketplaceSource::File { path } => path.clone(),
            _ => unreachable!("router 必须按 variant 分派"),
        };
        self.copy_into(source, dest, &path).await
    }

    /// 增量刷新:再 copy 一次到 dest(假设 source 文件还在,内容可能变)。
    async fn update(&self, source: &MarketplaceSource, dest: &Path) -> Result<()> {
        let path = match source {
            MarketplaceSource::File { path } => path.clone(),
            _ => unreachable!("router 必须按 variant 分派"),
        };
        self.copy_into(source, dest, &path).await?;
        Ok(())
    }
}

impl FileFetcher {
    /// 共享 copy 逻辑(给 fetch / update 复用)。
    async fn copy_into(
        &self,
        source: &MarketplaceSource,
        dest: &Path,
        path: &Path,
    ) -> Result<PathBuf> {
        if !path.is_file() {
            return Err(PluginError::MarketplaceFetch {
                kind: format!("{source:?}"),
                stderr: format!("marketplace.json 源文件不存在: {}", path.display()),
            });
        }

        // 清空 dest 再写。
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(|e| PluginError::StateIo {
                path: dest.to_path_buf(),
                source: e,
            })?;
        }
        let target_dir = dest.join(".claude-plugin");
        std::fs::create_dir_all(&target_dir).map_err(|e| PluginError::StateIo {
            path: target_dir.clone(),
            source: e,
        })?;
        let target = target_dir.join("marketplace.json");
        std::fs::copy(path, &target).map_err(|e| PluginError::StateIo {
            path: target.clone(),
            source: e,
        })?;
        Ok(dest.to_path_buf())
    }
}
