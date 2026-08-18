#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! `reflect-tools` —— Tool trait、内建工具,以及驱动 M2+ agent 循环的
//! 按并发安全性切分的 `ToolExecutionQueue`。

pub mod approval;
pub mod bm25;
pub mod builtins;
pub mod checkpoint;
pub mod plan_approval;
pub mod queue;
pub mod registry;
pub mod sandbox;
pub mod sanitize;
pub mod spec;
pub mod tool;
pub mod worktree;

pub use approval::{
    ApprovalGate, ApprovalWaiters, AskUserInputWaiters, AskUserQuestionWaiters, complete_approval,
    complete_ask_user_input, complete_ask_user_question,
};
pub use bm25::{Bm25Hit, rank_lines, tokenize};
pub use builtins::bash::{BashCommandClass, classify_command};
pub use plan_approval::{PlanApprovalGate, PlanApprovalWaiters, complete_plan_approval};
pub use queue::{ToolCallRequest, ToolExecutionQueue, ToolResult};
pub use registry::{ToolRegistry, ToolSource};
pub use sanitize::{SanitizeConfig, SanitizeError, Sanitizer, sanitize_text};
pub use spec::ToolSpec;
pub use tool::{Tool, ToolContext};
pub use worktree::{
    SessionWorktreeState, WorktreeCoordinator, create_worktree, default_worktree_path,
    detach_worktree, git_head_ref, git_root, remove_worktree, run_git, sanitize_branch_name,
    worktree_dirty,
};
// 从 protocol 重新导出,让下游用户能用
// `reflect_tools::ToolError` / `reflect_tools::ToolOutput`,
// 无需直接依赖 `reflect-protocol`。
pub use reflect_protocol::{ToolError, ToolOutput};

/// v1.x 功能 6:`AgentDefinition.readonly = true` 时从可见工具集中排除的
/// 变更类工具名。这些工具会修改文件系统 / 执行任意命令,只读 agent 不应拥有。
/// bash 因可执行任意写操作,整体排除(v1 不细分读写子命令)。
pub const READONLY_DENYLIST: &[&str] = &["write", "edit", "delete", "bash", "NotebookEdit"];
