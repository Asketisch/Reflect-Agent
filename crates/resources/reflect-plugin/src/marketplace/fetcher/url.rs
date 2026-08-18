//! `UrlFetcher` —— Phase E:HTTP GET marketplace.json。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use super::MarketplaceFetcher;
use crate::errors::{PluginError, Result};
use crate::manifest::MarketplaceSource;

/// Phase E:HTTP GET marketplace.json,落到 `<dest>/.claude-plugin/marketplace.json`。
///
/// 用 `reqwest = 0.12`,timeout 30s,非 2xx 报 `Http` 错。
pub struct UrlFetcher;

#[async_trait]
impl MarketplaceFetcher for UrlFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let (url, headers) = match source {
            MarketplaceSource::Url { url, headers } => (url.clone(), headers.clone()),
            _ => unreachable!("router 必须按 variant 分派"),
        };
        Self::download_and_write(&url, &headers, dest, source).await?;
        Ok(dest.to_path_buf())
    }

    /// 增量刷新:再 GET 一次并覆盖(简单策略;若服务器返回与上次完全相同,内容不变)。
    async fn update(&self, source: &MarketplaceSource, dest: &Path) -> Result<()> {
        // 与 fetch 行为一致 —— URL 拉取天然 idempotent。
        self.fetch(source, dest).await?;
        Ok(())
    }
}

impl UrlFetcher {
    /// HTTP GET 共享实现(给 fetch / update 复用)。
    async fn download_and_write(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        dest: &Path,
        source: &MarketplaceSource,
    ) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| PluginError::Http(format!("build reqwest client 失败: {e}")))?;
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let response = req
            .send()
            .await
            .map_err(|e| PluginError::Http(format!("GET {url} 失败: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::Http(format!(
                "GET {url} 返回 {}",
                status.as_u16()
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|e| PluginError::Http(format!("读 {url} body 失败: {e}")))?;

        // 写盘:清空 dest,落到 <dest>/.claude-plugin/marketplace.json。
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
        std::fs::write(&target, body).map_err(|e| PluginError::StateIo {
            path: target.clone(),
            source: e,
        })?;
        // 显式提示 parse 失败也以 fetch 成功为准(fetch 阶段不要求 manifest 合法,
        // marketplace_add 后才做 json parse 校验)。但若明显不是 JSON 也提示一下。
        if serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&target).map_err(
            |e| PluginError::StateIo {
                path: target.clone(),
                source: e,
            },
        )?)
        .is_err()
        {
            tracing::warn!(
                path = %target.display(),
                "marketplace.json 不是合法 JSON;后续 marketplace_add 解析可能失败"
            );
        }
        let _ = source; // 留作未来 metrics
        Ok(())
    }
}
