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

/// review 修复:校验来自远端 marketplace.json 的 git 参数,拒绝以 `-`
/// 开头的值 —— `--branch` / `checkout` / url 位置参数若以 `-` 开头会被
/// git 当作选项解析(如 `--upload-pack=<cmd>`、`-c core.fsmonitor=<cmd>`
/// 可在 clone 时执行任意命令)。只放行 git refname / URL 的常规字符。
fn is_safe_git_arg(value: &str) -> bool {
    !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/:@+~-".contains(c))
}

#[async_trait]
impl MarketplaceFetcher for GitFetcher {
    async fn fetch(&self, source: &MarketplaceSource, dest: &Path) -> Result<PathBuf> {
        let (url, git_ref, sha) = match source {
            MarketplaceSource::Git {
                url, r#ref, sha, ..
            } => (url.clone(), r#ref.clone(), sha.clone()),
            _ => unreachable!("router 必须按 variant 分派"),
        };

        // review 修复:ref / sha / url 来自远端 marketplace.json,先过参数
        // 安全校验(见 `is_safe_git_arg`),拒绝选项注入。
        for (label, value) in [
            ("ref", git_ref.as_deref()),
            ("sha", sha.as_deref()),
            ("url", Some(url.as_str())),
        ] {
            if let Some(v) = value
                && !is_safe_git_arg(v)
            {
                return Err(PluginError::MarketplaceFetch {
                    kind: "git".into(),
                    stderr: format!(
                        "{label} 参数含非法字符或以 '-' 开头,已拒绝(疑似选项注入): {v}"
                    ),
                });
            }
        }

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
