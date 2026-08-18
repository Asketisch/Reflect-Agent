//! `checkpoint` — v1.2 P0-3 git 快照与回退助手。
//!
//! 给 `CheckpointTool` / `RewindTool` 提供 git 操作原语,复用
//! [`crate::worktree::run_git`]。设计取舍(见 gap doc P0-3):
//! - **只回退工作区文件**(git tree),**不截断会话历史**。会话 JSONL
//!   append-only,rewind 只追加 `Rewind` marker(见
//!   `reflect_rollout::index::write_rewind_record`)。
//! - checkpoint 用 `git add -A` + `git commit` 拍快照;无 repo 时 `git init`。
//! - rewind 用 `git reset --hard <sha>` + `git clean -fd` 恢复文件树。
//!
//! 验证标准(gap doc):执行文件修改后 rewind 应恢复到修改前状态。

use std::path::Path;

use crate::ToolError;
use crate::worktree::run_git;

/// 在 `workspace` 下做 `git add -A` + `git commit`,返回新 HEAD 的 commit
/// sha。无变更时返回当前 HEAD sha(不报错)。无 git repo 时先 `git init`。
///
/// `msg` 作为 commit message;调用方通常传描述性标签(如 checkpoint 名)。
pub fn git_auto_commit(workspace: &Path, msg: &str) -> Result<String, ToolError> {
    ensure_repo(workspace)?;
    // git add -A(暂存所有变更,含新增 / 删除)。
    run_git(workspace, &["add", "-A"])?;
    // 尝试 commit;无变更时 commit 失败(exit 1),此时返回当前 HEAD sha。
    let commit_outcome = run_git(workspace, &["commit", "-m", msg]);
    if commit_outcome.is_err() {
        // 无变更(或 --amend 之外的 benign 失败)→ 返回当前 sha。
        return git_current_sha(workspace);
    }
    git_current_sha(workspace)
}

/// 当前 HEAD 的完整 commit sha(`git rev-parse HEAD`)。
pub fn git_current_sha(workspace: &Path) -> Result<String, ToolError> {
    ensure_repo(workspace)?;
    run_git(workspace, &["rev-parse", "HEAD"])
}

/// `git reset --hard <sha>` + `git clean -fd`,把工作区文件树强制恢复到
/// `sha` 指向的 commit 状态。**会丢弃所有未提交变更与未跟踪文件** ——
/// 调用方(工具层)应在用户审批后才调。
pub fn git_reset_hard(workspace: &Path, sha: &str) -> Result<(), ToolError> {
    ensure_repo(workspace)?;
    run_git(workspace, &["reset", "--hard", sha])?;
    run_git(workspace, &["clean", "-fd"])?;
    Ok(())
}

/// 校验 `sha` 是否是该 repo 的合法 commit(`git cat-file -e`)。
pub fn is_valid_commit(workspace: &Path, sha: &str) -> bool {
    ensure_repo(workspace).is_ok() && run_git(workspace, &["cat-file", "-e", sha]).is_ok()
}

/// 若 `workspace` 不是 git repo,`git init`(无操作若已是 repo)。
fn ensure_repo(workspace: &Path) -> Result<(), ToolError> {
    // `git rev-parse --git-dir` 在 repo 内返回 .git 路径,非 repo 时失败。
    if run_git(workspace, &["rev-parse", "--git-dir"]).is_err() {
        run_git(workspace, &["init"])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 用全局计数器给每个测试独立 tmp repo,避免并行污染。
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn tmp_repo() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "reflect_checkpoint_test_{}_{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // git init + 配 user(否则 commit 报错)。
        run_git(&dir, &["init"]).unwrap();
        run_git(&dir, &["config", "user.email", "test@test.test"]).unwrap();
        run_git(&dir, &["config", "user.name", "Test"]).unwrap();
        dir
    }

    #[test]
    fn auto_commit_creates_commit_and_returns_sha() {
        let repo = tmp_repo();
        fs::write(repo.join("a.txt"), "v1").unwrap();
        let sha1 = git_auto_commit(&repo, "first").unwrap();
        assert!(!sha1.is_empty());
        // sha 应是 40 字符的 hex。
        assert_eq!(sha1.len(), 40);
    }

    #[test]
    fn auto_commit_no_changes_returns_current_sha() {
        let repo = tmp_repo();
        fs::write(repo.join("a.txt"), "v1").unwrap();
        let sha1 = git_auto_commit(&repo, "first").unwrap();
        // 再 commit 无变更 → 返回同一 sha。
        let sha2 = git_auto_commit(&repo, "no-op").unwrap();
        assert_eq!(sha1, sha2);
    }

    #[test]
    fn reset_hard_restores_files_after_modification() {
        // 这是 gap doc 的核心验证用例:改文件 → checkpoint → 再改 → rewind
        // → 内容恢复到 checkpoint 状态。
        let repo = tmp_repo();
        fs::write(repo.join("a.txt"), "original").unwrap();
        let sha = git_auto_commit(&repo, "checkpoint").unwrap();

        // 修改文件。
        fs::write(repo.join("a.txt"), "modified").unwrap();
        fs::write(repo.join("b.txt"), "untracked").unwrap();
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "modified");

        // rewind 到 checkpoint sha。
        git_reset_hard(&repo, &sha).unwrap();

        // a.txt 恢复原内容,b.txt(未跟踪)被 clean 删掉。
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "original",
            "rewind must restore checkpoint content"
        );
        assert!(!repo.join("b.txt").exists(), "clean -fd removes untracked");
    }

    #[test]
    fn is_valid_commit_distinguishes_known_and_unknown() {
        let repo = tmp_repo();
        fs::write(repo.join("a.txt"), "v1").unwrap();
        let sha = git_auto_commit(&repo, "x").unwrap();
        assert!(is_valid_commit(&repo, &sha));
        assert!(!is_valid_commit(&repo, "not-a-real-sha"));
    }

    #[test]
    fn ensure_repo_inits_when_absent() {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("reflect_ckpt_noinit_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 非 repo → ensure_repo 应 init。
        ensure_repo(&dir).unwrap();
        assert!(run_git(&dir, &["rev-parse", "--git-dir"]).is_ok());
    }
}
