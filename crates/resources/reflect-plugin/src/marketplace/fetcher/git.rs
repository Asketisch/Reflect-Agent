//! `GitFetcher` —— 通过 shell `git` CLI 拉取 marketplace 仓库。

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use super::MarketplaceFetcher;
use super::util::{tmp_sibling, truncate_stderr};
use crate::errors::{PluginError, Result};
use crate::manifest::MarketplaceSource;

/// 通过 shell `git` CLI 拉取 marketplace 仓库。
///
/// 具体调用:
/// ```text
/// git clone --depth 1 --filter=blob:none [--branch <ref>] <url> <dest.tmp>
/// [git -C <dest.tmp> checkout <sha>]   # 仅当 ref=None 且 sha=Some
/// fs::rename(dest.tmp, dest)
/// ```
///
/// 失败模式:
/// - `git` 不在 PATH → `Io`
/// - `git` exit 非 0 → `Git(stderr)`
/// - dest 已存在 → 先 `remove_dir_all` 再 clone(简化 reinstall 路径)
pub struct GitFetcher;

#[async_trait]
impl MarketplaceFetcher for GitFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let (url, git_ref, sha) = match source {
            MarketplaceSource::Git {
                url, r#ref, sha, ..
            } => (url.clone(), r#ref.clone(), sha.clone()),
            _ => unreachable!("router 必须按 variant 分派"),
        };

        // 1. 清空 dest(若存在),保证 clone 是从空目录起步。
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(|e| PluginError::StateIo {
                path: dest.to_path_buf(),
                source: e,
            })?;
        }
        let tmp = tmp_sibling(dest);

        // 2. git clone(走 shell `git`,所以 bin 必须在 PATH)。
        // 不加 `--single-branch`:remote HEAD 在 fresh push 时未设,会失败。
        let mut cmd = Command::new("git");
        cmd.arg("clone")
            .arg("--depth")
            .arg("1")
            .arg("--filter=blob:none");
        if let Some(r) = &git_ref {
            cmd.arg("--branch").arg(r);
        }
        cmd.arg(&url).arg(&tmp);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .await
            .map_err(|e| PluginError::Git(format!("spawn `git clone` 失败: {e}")))?;
        if !output.status.success() {
            // best-effort 清理 tmp(可能部分写入)。
            let _ = std::fs::remove_dir_all(&tmp);
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(PluginError::Git(format!(
                "`git clone {url}` exit {:?}: {}",
                output.status.code(),
                truncate_stderr(&stderr, 4_000)
            )));
        }

        // 3. 可选 checkout sha(ref 与 sha 二选一;ref 已用 --branch 锁住)。
        if git_ref.is_none()
            && let Some(sha) = &sha
        {
            let mut cmd = Command::new("git");
            cmd.current_dir(&tmp).arg("checkout").arg(sha);
            cmd.stdout(Stdio::null()).stderr(Stdio::piped());
            let output = cmd
                .output()
                .await
                .map_err(|e| PluginError::Git(format!("spawn `git checkout` 失败: {e}")))?;
            if !output.status.success() {
                let _ = std::fs::remove_dir_all(&tmp);
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                return Err(PluginError::Git(format!(
                    "`git checkout {sha}` exit {:?}: {}",
                    output.status.code(),
                    truncate_stderr(&stderr, 4_000)
                )));
            }
        }

        // 4. 原子 rename 到 dest。
        std::fs::rename(&tmp, dest).map_err(|e| PluginError::StateIo {
            path: dest.to_path_buf(),
            source: e,
        })?;
        Ok(dest.to_path_buf())
    }

    /// 增量刷新:在已 clone 的 `dest` 上 `git pull --ff-only`,fast-forward 失败 → 报 Git 错。
    ///
    /// 不动 cache dir 结构(只动 .git/),破坏性回退留给上层(用户重 `marketplace add`)。
    async fn update(&self, _source: &MarketplaceSource, dest: &Path) -> Result<()> {
        if !dest.exists() {
            return Err(PluginError::MarketplaceFetch {
                kind: "git".into(),
                stderr: format!("update: dest {} 不存在(需先 fetch)", dest.display()),
            });
        }
        let mut cmd = Command::new("git");
        cmd.current_dir(dest).arg("pull").arg("--ff-only");
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .await
            .map_err(|e| PluginError::Git(format!("spawn `git pull` 失败: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(PluginError::Git(format!(
                "`git pull` exit {:?}: {}",
                output.status.code(),
                truncate_stderr(&stderr, 4_000)
            )));
        }
        Ok(())
    }
}
