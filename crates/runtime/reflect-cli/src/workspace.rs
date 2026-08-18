//! `reflect workspace` —— Git 克隆 / 同步 CLI。

use std::path::PathBuf;

use reflect_stream::workspace_sync::{WorkspaceSyncOptions, clone_repo, sync_status_line};

/// 克隆远程仓库到本地目录。
pub fn run_clone(
    url: String,
    dest: PathBuf,
    branch: Option<String>,
    depth: u32,
) -> anyhow::Result<()> {
    println!("Reflect workspace clone");
    println!("======================");
    let opts = WorkspaceSyncOptions {
        url,
        dest: dest.clone(),
        branch,
        depth,
    };
    let path = clone_repo(&opts)?;
    println!("✓ cloned to {}", path.display());
    Ok(())
}

pub fn run_ls_hint() -> anyhow::Result<()> {
    println!("{}", sync_status_line());
    Ok(())
}
