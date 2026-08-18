//! Phase 1 + Phase 2 + Phase 4 内置工具:`TaskCreate` / `TaskGet` / `TaskUpdate` /
//! `TaskList` / `TaskStop` / `TaskOutput` / `TaskClaim` / `TaskRelease` /
//! `TodoWrite`(V1) + `TeamCreate` / `TeamDelete`。
//!
//! 工具设计要点(参见 `crates/reflect-tools/src/builtins/echo.rs` 与
//! `crates/reflect-subagent/src/tools/mod.rs::CallSubAgentTool`):
//!
//! - 全部以 `ToolSource::Builtin` 注册,落到 `ToolRegistry::register_with_source`。
//! - 全部 `pub struct FooTool { manager: Arc<TaskManager> }`,stateful 模式。
//! - `TaskUpdate` / `TeamCreate` / `TeamDelete` 走 `required_permission = Prompt`,
//!   因它们改持久化状态;其他 task 工具为 `Auto`(manager 内部有 per-key 锁,
//!   重复调用天然安全)。`TodoWrite` 是纯内存操作,也是 `Auto`。
//! - `TaskOutput` 读 `task.output_path` 文件 —— Phase 0 `TaskManager::create_task`
//!   已经自动分配,工具内只是 `tokio::fs::read_to_string`,带 `block` / `timeout_ms`。
//! - `is_concurrency_safe`:TaskCreate/TaskGet/TaskList/TaskOutput/TaskClaim 标 `true`
//!   (只读或写独立 id),TaskUpdate/TaskStop/TodoWrite/TeamCreate/TeamDelete/TaskRelease
//!   标 `false`(副作用跨调用顺序敏感)。
//! - `TaskClaim` / `TaskRelease` 是 v1.1.0 Phase 4 协调器派工协议,worker 必须
//!   能调用,所以默认都进 `ToolRegistry`;`bootstrap_m4` 在 coordinator 模式
//!   启用时再额外注入 `WriteNote` / `ReadNotes` 工具(scratchpad 协议)。

pub mod note_read;
pub mod note_write;
pub mod task_claim;
pub mod task_create;
pub mod task_get;
pub mod task_list;
pub mod task_output;
pub mod task_release;
pub mod task_stop;
pub mod task_update;
pub mod team_create;
pub mod team_delete;
pub mod todo_write;

pub use note_read::ReadNotesTool;
pub use note_write::WriteNoteTool;
pub use task_claim::TaskClaimTool;
pub use task_create::TaskCreateTool;
pub use task_get::TaskGetTool;
pub use task_list::TaskListTool;
pub use task_output::TaskOutputTool;
pub use task_release::TaskReleaseTool;
pub use task_stop::TaskStopTool;
pub use task_update::TaskUpdateTool;
pub use team_create::TeamCreateTool;
pub use team_delete::TeamDeleteTool;
pub use todo_write::{TodoRow, TodoWriteTool};

use std::sync::Arc;

use reflect_tools::{ToolError, ToolRegistry, ToolSource};

use crate::manager::TaskManager;

/// Phase 1 + Phase 2 + Phase 4 内置工具集体注册入口。
///
/// `reflect-exec` 启动时调用一次,把 11 个工具挂到主 `ToolRegistry`。
/// `ToolRegistry::register_with_source` 不返回 `Result`(v1.0.0-rc2 起),
/// 所以这里也不返回。`WriteNote` / `ReadNotes` 工具因需要 scratchpad 路径,
/// 不走 `register_all`,由 `reflect-exec::bootstrap_m4` 在 coordinator 模式
/// 启用后显式 `registry.register_with_source(...)`。
pub fn register_all(registry: &ToolRegistry, manager: Arc<TaskManager>) {
    // ── task 工具 (Phase 1) ──
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskCreateTool::new(manager.clone())),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskGetTool::new(manager.clone())),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskUpdateTool::new(manager.clone())),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskListTool::new(manager.clone())),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskStopTool::new(manager.clone())),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskOutputTool::new(manager.clone())),
    );
    // ── TaskClaim / TaskRelease (Phase 4 协调器派工协议) ──
    // 默认 self_agent_id 是空串,worker 调用时通过 `claimerId` 覆盖;
    // `reflect-exec` 在 spawn worker 时会用真实 agent id 构造一份,
    // 覆盖这里的占位(参见 Step 7 bootstrap_m4 注入逻辑)。
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskClaimTool::new(manager.clone(), "")),
    );
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TaskReleaseTool::new(manager.clone())),
    );
    // ── TodoWrite(Phase 1,V1 内存版)──
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TodoWriteTool::new(manager.clone())),
    );
    // ── team 工具 (Phase 2) ──
    registry.register_with_source(
        ToolSource::Builtin,
        Arc::new(TeamCreateTool::new(manager.clone())),
    );
    registry.register_with_source(ToolSource::Builtin, Arc::new(TeamDeleteTool::new(manager)));
}

/// 工具公共 helper:从 JSON args 中读取必填的 `u32` 字段。
#[inline]
pub fn parse_u32_arg(args: &serde_json::Value, key: &str) -> Result<u32, ToolError> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("missing or non-u32 '{key}'"),
        })
}

/// 工具公共 helper:从 JSON args 中读取必填字符串字段。
#[inline]
pub fn parse_string_arg(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("missing or non-string '{key}'"),
        })
}

/// 工具公共 helper:从 JSON args 中读取可选字符串字段。
#[inline]
pub fn parse_optional_string(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 决定 list_id:显式 `listId` 优先,否则默认用 `ctx.session_id`。
///
/// 注释:`ToolContext.session_id` 是 `ThreadId`(UUID-style),直接 `.to_string()`
/// 作为 list_id。这与 `reflect-rollout` 的会话文件命名空间同源,便于交叉
/// 引用(同一 session 的 todos / tasks 落同一目录)。
#[inline]
pub fn list_id_from_args_or_ctx(
    args: &serde_json::Value,
    ctx: &reflect_tools::ToolContext,
) -> String {
    parse_optional_string(args, "listId").unwrap_or_else(|| ctx.session_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryTaskStore, InMemoryTeamStore};
    use std::collections::HashSet;

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    #[test]
    fn parse_u32_arg_present() {
        let args = serde_json::json!({"taskId": 7});
        assert_eq!(parse_u32_arg(&args, "taskId").unwrap(), 7);
    }

    #[test]
    fn parse_u32_arg_missing_errors() {
        let args = serde_json::json!({});
        let err = parse_u32_arg(&args, "taskId").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn parse_string_arg_present() {
        let args = serde_json::json!({"subject": "x"});
        assert_eq!(parse_string_arg(&args, "subject").unwrap(), "x");
    }

    #[test]
    fn parse_optional_string_none() {
        let args = serde_json::json!({});
        assert_eq!(parse_optional_string(&args, "listId"), None);
    }

    #[test]
    fn list_id_falls_back_to_ctx_session() {
        let args = serde_json::json!({});
        let ctx = reflect_tools::ToolContext::default();
        assert_eq!(
            list_id_from_args_or_ctx(&args, &ctx),
            ctx.session_id.to_string()
        );
    }

    #[test]
    fn list_id_explicit_overrides_ctx() {
        let args = serde_json::json!({"listId": "rocket"});
        let ctx = reflect_tools::ToolContext::default();
        assert_eq!(list_id_from_args_or_ctx(&args, &ctx), "rocket");
    }

    #[test]
    fn register_all_contains_eleven_tools() {
        let registry = ToolRegistry::new();
        register_all(&registry, mgr());
        let names: HashSet<String> = registry.list().into_iter().collect();
        let expected: HashSet<&str> = [
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
            "TaskStop",
            "TaskOutput",
            "TaskClaim",
            "TaskRelease",
            "TodoWrite",
            "TeamCreate",
            "TeamDelete",
        ]
        .into_iter()
        .collect();
        let actual: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            actual, expected,
            "registry should contain exactly the 11 phase 1+2+4 tools"
        );
    }
}
