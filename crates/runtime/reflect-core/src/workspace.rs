//! 工作区 / 项目根探测。
//!
//! `reflect` 默认用进程 cwd 当工作区。`--auto-root` 开启后,`detect_project_root`
//! 会从 cwd 向上查找项目根(第一个含 `.git` / `Cargo.toml` / `.reflect/` 的目录),
//! 避免「在子目录里启动 → 工作区过窄」。找不到任何标记则原样返回 `start`。

use std::path::{Path, PathBuf};

/// 从 `start` 向上查找项目根:第一个含 `.git` / `Cargo.toml` / `.reflect/`
/// 的目录。任一标记命中即停。找不到则返回 `start` 本身(不变)。
///
/// `start` 一般是 `std::env::current_dir()` 的结果。相对路径会先 canonicalize
/// (失败则原样使用),保证 `pop()` 能正确向上走。
pub fn detect_project_root(start: &Path) -> PathBuf {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        start.canonicalize().unwrap_or_else(|_| start.to_path_buf())
    };
    loop {
        for marker in [".git", "Cargo.toml", ".reflect"] {
            if cur.join(marker).exists() {
                return cur;
            }
        }
        if !cur.pop() {
            // 走到文件系统根仍未命中 → 退回原 start,避免悄悄改写工作区。
            return start.to_path_buf();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 辅助:在 `root` 下建一个临时 git 仓库目录结构并返回其路径。
    fn mkdir_with_git(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join(".git")).unwrap();
        root.to_path_buf()
    }

    #[test]
    fn finds_git_marker_by_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdir_with_git(tmp.path());
        // 在 root 之下嵌两层子目录,模拟「在子目录里启动」。
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(detect_project_root(&deep), root);
    }

    #[test]
    fn finds_cargo_toml_when_no_git() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        let deep = root.join("pkg").join("src");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(detect_project_root(&deep), root);
    }

    #[test]
    fn returns_start_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        // tmp 目录及其祖先(在 CI/测试机临时目录里)通常没有项目标记;
        // 关键不变量:返回值 == start,绝不悄悄上跳。
        let start = tmp.path().to_path_buf();
        let detected = detect_project_root(&start);
        assert_eq!(detected, start);
    }

    #[test]
    fn start_itself_with_marker_returns_start() {
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdir_with_git(tmp.path());
        // start 自身就含标记 → 直接返回 start(不变)。
        assert_eq!(detect_project_root(&root), root);
    }
}
