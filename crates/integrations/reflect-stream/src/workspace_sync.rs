//! Workspace Clone/Sync —— Git 仓库克隆到本地 workspace。

use std::path::{Path, PathBuf};
use std::process::Command;

/// Git clone 选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSyncOptions {
    /// 远程 URL。
    pub url: String,
    /// 目标目录(不存在则创建父目录)。
    pub dest: PathBuf,
    /// 可选 branch / tag。
    pub branch: Option<String>,
    /// `--depth` 浅克隆(0 = 全量)。
    pub depth: u32,
}

/// 克隆 Git 仓库到 `dest`。
///
/// 使用 `git clone`;失败返回 stderr 摘要。
pub fn clone_repo(opts: &WorkspaceSyncOptions) -> anyhow::Result<PathBuf> {
    if opts.url.trim().is_empty() {
        anyhow::bail!("workspace-sync: url 不能为空");
    }
    if let Some(parent) = opts.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    if opts.depth > 0 {
        cmd.arg("--depth").arg(opts.depth.to_string());
    }
    if let Some(ref b) = opts.branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(&opts.url).arg(&opts.dest);
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git clone 失败: {err}");
    }
    Ok(opts.dest.clone())
}

/// 检查路径是否为 git 仓库。
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

pub fn sync_status_line() -> String {
    "workspace-sync: `reflect workspace clone <url> [--dest DIR]`".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_url_errors() {
        let opts = WorkspaceSyncOptions {
            url: "".into(),
            dest: PathBuf::from("/tmp/x"),
            branch: None,
            depth: 0,
        };
        assert!(clone_repo(&opts).is_err());
    }

    #[test]
    fn is_git_repo_false_for_tmp() {
        assert!(!is_git_repo(Path::new("/tmp/nonexistent-repo-xyz")));
    }
}
