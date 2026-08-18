//! Git worktree 会话状态与辅助函数。
//!
//! `EnterWorktreeTool` / `ExitWorktreeTool` 通过 `ToolContext` 与
//! `AgentConfig` 共享的 `Arc<RwLock<…>>` 热切换 workspace,并在
//! `SessionWorktreeState` 里记录原始路径以便退出时恢复。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::tool::ToolError;

/// 单次 worktree 隔离会话的快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorktreeState {
    /// 进入 worktree 前的 workspace(通常是主仓库根或子目录)。
    pub original_workspace: PathBuf,
    /// `git worktree add` 创建的工作树路径。
    pub worktree_path: PathBuf,
    /// 临时分支名。
    pub branch: String,
}

/// 在 `cwd` 下执行 git 子命令,失败时映射为 `ToolError::Execution`。
pub fn run_git(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| ToolError::Execution(format!("failed to spawn git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(ToolError::Execution(format!(
            "git {} failed (exit {}): {}{}",
            args.join(" "),
            output.status,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" | stdout: {}", stdout.trim())
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 解析 git 仓库根目录。
pub fn git_root(cwd: &Path) -> Result<PathBuf, ToolError> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root))
}

/// 当前 HEAD 的短 ref(用于默认 base)。
pub fn git_head_ref(cwd: &Path) -> Result<String, ToolError> {
    run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// 工作树是否有未提交变更(`git status --porcelain` 非空)。
pub fn worktree_dirty(path: &Path) -> Result<bool, ToolError> {
    let out = run_git(path, &["status", "--porcelain"])?;
    Ok(!out.trim().is_empty())
}

/// 生成分支名:优先用用户输入,否则 `reflect/wt-{8 hex}`。
pub fn sanitize_branch_name(raw: Option<&str>) -> String {
    if let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    format!("reflect/wt-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

/// 默认 worktree 目录:`{git_root}/.reflect/worktrees/{branch_sanitized}`。
pub fn default_worktree_path(git_root: &Path, branch: &str) -> PathBuf {
    let slug = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() { "wt".into() } else { slug };
    git_root.join(".reflect").join("worktrees").join(slug)
}

/// 创建 worktree 并返回 `(worktree_path, branch)`。
pub fn create_worktree(
    git_root: &Path,
    branch: &str,
    worktree_path: &Path,
    base_ref: Option<&str>,
) -> Result<(), ToolError> {
    std::fs::create_dir_all(
        worktree_path
            .parent()
            .ok_or_else(|| ToolError::Execution("invalid worktree parent path".into()))?,
    )
    .map_err(|e| ToolError::Io(e.to_string()))?;

    let base = base_ref
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("HEAD");
    run_git(
        git_root,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree_path
                .to_str()
                .ok_or_else(|| ToolError::Execution("worktree path is not valid UTF-8".into()))?,
            base,
        ],
    )?;
    Ok(())
}

/// 退出 worktree:`keep=false` 时强制清理 worktree 并删除分支;
/// `keep=true` 时仅断开 agent 关联,worktree 保留在磁盘上。
pub fn detach_worktree(
    git_root: &Path,
    state: &SessionWorktreeState,
    keep: bool,
) -> Result<(), ToolError> {
    if keep {
        return Ok(());
    }
    let _ = run_git(&state.worktree_path, &["reset", "--hard"]);
    let _ = run_git(&state.worktree_path, &["clean", "-fd"]);
    let path_str = state
        .worktree_path
        .to_str()
        .ok_or_else(|| ToolError::Execution("worktree path is not valid UTF-8".into()))?;
    run_git(git_root, &["worktree", "remove", "--force", path_str])?;
    let _ = run_git(git_root, &["branch", "-D", &state.branch]);
    Ok(())
}

/// 测试 /  teardown 用:无论 `keep` 都删除 worktree。
pub fn remove_worktree(
    git_root: &Path,
    state: &SessionWorktreeState,
    keep: bool,
) -> Result<(), ToolError> {
    detach_worktree(git_root, state, keep)?;
    if keep {
        let path_str = state
            .worktree_path
            .to_str()
            .ok_or_else(|| ToolError::Execution("worktree path is not valid UTF-8".into()))?;
        run_git(git_root, &["worktree", "remove", path_str])?;
    }
    Ok(())
}

// ── P2 git-worktree-auto: Coordinator 自动 worktree ─────────────────────

use std::collections::HashMap;
use std::sync::Mutex;

/// Coordinator 模式下 per-task worktree 规划(stub:路径规划,可选真实 git)。
#[derive(Debug)]
pub struct WorktreeCoordinator {
    git_root: PathBuf,
    entries: Mutex<HashMap<String, SessionWorktreeState>>,
}

impl WorktreeCoordinator {
    pub fn new(git_root: impl Into<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 为 worker task 确保 worktree;`create_git=false` 时仅返回规划路径。
    pub fn ensure_for_task(
        &self,
        task_id: &str,
        create_git: bool,
    ) -> Result<SessionWorktreeState, ToolError> {
        let mut g = self.entries.lock().unwrap();
        if let Some(e) = g.get(task_id) {
            return Ok(e.clone());
        }
        let branch = format!("reflect/task-{task_id}");
        let worktree_path = default_worktree_path(&self.git_root, &branch);
        if create_git {
            create_worktree(&self.git_root, &branch, &worktree_path, None)?;
        }
        let state = SessionWorktreeState {
            original_workspace: self.git_root.clone(),
            worktree_path,
            branch,
        };
        g.insert(task_id.to_string(), state.clone());
        Ok(state)
    }

    pub fn remove_task(&self, task_id: &str, keep: bool) -> Result<bool, ToolError> {
        let mut g = self.entries.lock().unwrap();
        if let Some(state) = g.remove(task_id) {
            if state.worktree_path.exists() {
                detach_worktree(&self.git_root, &state, keep)?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub fn list(&self) -> Vec<SessionWorktreeState> {
        let mut v: Vec<_> = self.entries.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.branch.cmp(&b.branch));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;

    fn init_git_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init"]).expect("git init");
        run_git(dir, &["config", "user.email", "test@example.com"]).unwrap();
        run_git(dir, &["config", "user.name", "test"]).unwrap();
        fs::write(dir.join("README.md"), "hello").unwrap();
        run_git(dir, &["add", "README.md"]).unwrap();
        run_git(dir, &["commit", "-m", "init"]).unwrap();
    }

    #[test]
    fn sanitize_branch_name_defaults_to_reflect_prefix() {
        let name = sanitize_branch_name(None);
        assert!(name.starts_with("reflect/wt-"));
    }

    #[test]
    fn default_worktree_path_uses_reflect_subdir() {
        let root = PathBuf::from("/tmp/repo");
        let p = default_worktree_path(&root, "feature/foo");
        assert!(p.starts_with(root.join(".reflect/worktrees")));
    }

    #[test]
    fn create_and_remove_worktree_roundtrip() {
        let dir = std::env::temp_dir().join(format!("reflect_wt_test_{}", uuid::Uuid::new_v4()));
        if dir.exists() {
            fs::remove_dir_all(&dir).ok();
        }
        init_git_repo(&dir);
        let root = git_root(&dir).unwrap();
        let branch = "reflect/test-branch";
        let wt_path = default_worktree_path(&root, branch);
        create_worktree(&root, branch, &wt_path, None).unwrap();
        assert!(wt_path.join("README.md").exists());

        let state = SessionWorktreeState {
            original_workspace: dir.clone(),
            worktree_path: wt_path.clone(),
            branch: branch.to_string(),
        };
        remove_worktree(&root, &state, true).unwrap();
        assert!(!wt_path.exists());

        let _ = StdCommand::new("rm").arg("-rf").arg(&dir).status();
    }

    #[test]
    fn coordinator_plans_path_without_git() {
        let dir = std::env::temp_dir().join(format!("reflect_wt_coord_{}", uuid::Uuid::new_v4()));
        init_git_repo(&dir);
        let root = git_root(&dir).unwrap();
        let c = WorktreeCoordinator::new(root.clone());
        let state = c.ensure_for_task("99", false).unwrap();
        assert!(state.branch.contains("99"));
        assert!(
            state
                .worktree_path
                .starts_with(root.join(".reflect/worktrees"))
        );
        let _ = StdCommand::new("rm").arg("-rf").arg(&dir).status();
    }
}
