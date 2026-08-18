//! `EnterWorktree` —— 创建临时分支 + git worktree,热切换 agent workspace。

use std::path::PathBuf;

use async_trait::async_trait;
use reflect_protocol::PermissionMode;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};
use crate::worktree::{
    SessionWorktreeState, create_worktree, default_worktree_path, git_root, sanitize_branch_name,
};

pub struct EnterWorktreeTool;

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "EnterWorktree"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.EnterWorktree")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "新分支名;省略时自动生成 reflect/wt-<id>"
                },
                "base_ref": {
                    "type": "string",
                    "description": "新分支的起点 ref,默认 HEAD"
                },
                "path": {
                    "type": "string",
                    "description": "worktree 目录;省略时用 <repo>/.reflect/worktrees/<branch>"
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
        if ctx.worktree_state().is_some() {
            return Err(ToolError::Execution(
                "already in a worktree session; call ExitWorktree first".into(),
            ));
        }

        let original = ctx.workspace_path();
        let git_root = git_root(&original)?;
        let branch = sanitize_branch_name(args.get("branch").and_then(|v| v.as_str()));
        let base_ref = args.get("base_ref").and_then(|v| v.as_str());
        let worktree_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_worktree_path(&git_root, &branch));

        if worktree_path.exists() {
            return Err(ToolError::Execution(format!(
                "worktree path already exists: {}",
                worktree_path.display()
            )));
        }

        create_worktree(&git_root, &branch, &worktree_path, base_ref)?;

        let canonical_wt = worktree_path
            .canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?;
        let state = SessionWorktreeState {
            original_workspace: original.clone(),
            worktree_path: canonical_wt.clone(),
            branch: branch.clone(),
        };
        ctx.set_worktree_state(Some(state));
        ctx.set_workspace(canonical_wt.clone());

        let base = base_ref.unwrap_or("HEAD");
        Ok(reflect_protocol::ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Entered worktree at {} on branch {} (base {base})",
                canonical_wt.display(),
                branch
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "branch": branch,
                "worktree_path": canonical_wt,
                "original_workspace": original,
                "base_ref": base,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::git_head_ref;
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
    async fn enter_worktree_switches_workspace() {
        let dir = std::env::temp_dir().join(format!("reflect_enter_wt_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        init_repo(&dir);
        let ctx = ToolContext::for_workspace(dir.clone());
        let out = EnterWorktreeTool
            .execute(
                ctx.clone(),
                serde_json::json!({"branch": "reflect/enter-test"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let wt = ctx.workspace_path();
        assert_ne!(wt, dir);
        assert!(wt.join("a.txt").exists());
        assert!(ctx.worktree_state().is_some());
        // 清理
        if let Some(state) = ctx.worktree_state() {
            let root = crate::worktree::git_root(&dir).unwrap();
            crate::worktree::remove_worktree(&root, &state, true).ok();
        }
        let _ = StdCommand::new("rm").arg("-rf").arg(&dir).status();
    }

    #[test]
    fn enter_worktree_metadata_is_stable() {
        let t = EnterWorktreeTool;
        assert_eq!(t.name(), "EnterWorktree");
        assert!(!t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Prompt);
    }

    #[tokio::test]
    async fn enter_worktree_rejects_nested_session() {
        let ctx = ToolContext::for_workspace("/tmp");
        ctx.set_worktree_state(Some(SessionWorktreeState {
            original_workspace: "/tmp".into(),
            worktree_path: "/tmp/wt".into(),
            branch: "b".into(),
        }));
        let err = EnterWorktreeTool
            .execute(ctx, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn git_head_ref_reads_branch_in_repo() {
        let dir = std::env::temp_dir().join(format!("reflect_head_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        init_repo(&dir);
        let head = git_head_ref(&dir).unwrap();
        assert!(!head.is_empty());
        let _ = StdCommand::new("rm").arg("-rf").arg(&dir).status();
    }
}
