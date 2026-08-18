//! `ExitWorktree` —— 检查变更,保留或清理 worktree,恢复原始 workspace。

use async_trait::async_trait;
use reflect_protocol::PermissionMode;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};
use crate::worktree::{detach_worktree, git_root, worktree_dirty};

pub struct ExitWorktreeTool;

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.ExitWorktree")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "keep": {
                    "type": "boolean",
                    "description": "true(默认):保留 worktree 与分支,仅切回 workspace;\
                                    false:丢弃变更并删除 worktree + 分支"
                }
            },
            "required": []
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: Value,
    ) -> Result<reflect_protocol::ToolOutput, ToolError> {
        let state = ctx
            .worktree_state()
            .ok_or_else(|| ToolError::Execution("not in a worktree session".into()))?;
        let keep = args.get("keep").and_then(|v| v.as_bool()).unwrap_or(true);
        let dirty = worktree_dirty(&state.worktree_path)?;
        let git_root = git_root(&state.original_workspace)?;

        detach_worktree(&git_root, &state, keep)?;

        ctx.set_workspace(state.original_workspace.clone());
        ctx.set_worktree_state(None);

        let msg = if dirty {
            if keep {
                "Exited worktree (dirty tree kept on disk; workspace restored)".to_string()
            } else {
                "Exited worktree (discarded dirty changes; worktree removed)".to_string()
            }
        } else if keep {
            "Exited worktree (clean; worktree kept on disk; workspace restored)".to_string()
        } else {
            "Exited worktree (clean; worktree and branch removed; workspace restored)".to_string()
        };

        Ok(reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(msg)],
            is_error: false,
            metadata: serde_json::json!({
                "keep": keep,
                "was_dirty": dirty,
                "restored_workspace": state.original_workspace,
                "former_worktree": state.worktree_path,
                "branch": state.branch,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::{SessionWorktreeState, create_worktree, default_worktree_path};
    use std::fs;
    use std::process::Command as StdCommand;

    fn init_repo(dir: &std::path::Path) {
        crate::worktree::run_git(dir, &["init"]).unwrap();
        crate::worktree::run_git(dir, &["config", "user.email", "t@e.com"]).unwrap();
        crate::worktree::run_git(dir, &["config", "user.name", "t"]).unwrap();
        fs::write(dir.join("a.txt"), "x").unwrap();
        crate::worktree::run_git(dir, &["add", "a.txt"]).unwrap();
        crate::worktree::run_git(dir, &["commit", "-m", "init"]).unwrap();
    }

    #[tokio::test]
    async fn exit_worktree_restores_workspace() {
        let dir = std::env::temp_dir().join(format!("reflect_exit_wt_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        init_repo(&dir);
        let root = crate::worktree::git_root(&dir).unwrap();
        let branch = "reflect/exit-test";
        let wt_path = default_worktree_path(&root, branch);
        create_worktree(&root, branch, &wt_path, None).unwrap();
        let canonical_wt = wt_path.canonicalize().unwrap();

        let workspace = std::sync::Arc::new(parking_lot::RwLock::new(dir.clone()));
        let worktree = std::sync::Arc::new(parking_lot::RwLock::new(Some(SessionWorktreeState {
            original_workspace: dir.clone(),
            worktree_path: canonical_wt.clone(),
            branch: branch.to_string(),
        })));
        let ctx = ToolContext::with_shared(workspace, worktree);
        *ctx.workspace.write() = canonical_wt;

        let out = ExitWorktreeTool
            .execute(ctx.clone(), serde_json::json!({"keep": true}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(ctx.workspace_path(), dir);
        assert!(ctx.worktree_state().is_none());

        let _ = StdCommand::new("rm").arg("-rf").arg(&dir).status();
    }

    #[tokio::test]
    async fn exit_worktree_errors_when_not_in_session() {
        let ctx = ToolContext::for_workspace("/tmp");
        let err = ExitWorktreeTool
            .execute(ctx, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn exit_worktree_metadata_is_stable() {
        let t = ExitWorktreeTool;
        assert_eq!(t.name(), "ExitWorktree");
        assert!(!t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Prompt);
    }
}
