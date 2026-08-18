//! `reflect-task` — 结构化任务管理 (Task*) + 团队管理 (TeamCreate/Delete) + TodoWrite + Coordinator 模式。
//!
//! v1.1.0 引入。落地为 Rust builtin tools。
//!
//! ## 设计原则
//!
//! - **状态文档 ≠ 事件流**:Task / Team 持久化到 `~/.reflect/tasks/<list>/<id>.json` 与
//!   `~/.reflect/teams/<name>.json`,不复用 `reflect-rollout` 的 JSONL 事件流。
//! - **钩子优先**:`HookEventKind::TaskCreated` / `TaskCompleted` / `TaskUpdated` 三个
//!   新事件,允许外部 hook 订阅任务生命周期(团队生命周期走 ToolCallEnd 透传)。
//! - **Coordinator 模式可配置**:258 行 system prompt 拆为 `coordinator_prompt.md` 资源,
//!   通过 `~/.reflect/config.toml` 的 `[coordinator]` 表覆盖。
//! - **TodoWrite(V1) 与 Task 系统(V2) 并存**:TodoWrite 走 `ToolContext.metadata["todos"]`
//!   + TUI 单行渲染,Task* 走 `TaskManager` 持久化。
//!
//! ## 模块组织
//!
//! - [`model`] — `Task` / `TaskStatus` / `TeamFile` / `TeamMemberSpec` 数据结构。
//! - [`error`] — `TaskError` 统一错误类型。
//! - [`team`] — `lead_agent_id_for` / `parse_agent_id` / `validate_team_name` helper。
//! - [`store`] — `TaskStore` trait + `InMemoryTaskStore` + `FileTaskStore`。
//! - [`team_store`] — `TeamStore` trait + `InMemoryTeamStore` + `FileTeamStore`。
//! - [`manager`] — `TaskManager` 高层 API,组合 stores + 钩子触发 + event_sink。
//! - [`tools`] — 9 个 builtin tools(Task* / TodoWrite / Team*)。
//! - [`coordinator`] — Coordinator 模式配置 / worker 工具白名单 / scratchpad 路径
//!   / 默认 system prompt(`v1.1.0` Phase 4)。
//!
//! Coordinator 模式在 `coordinator.rs` 子模块。

pub mod coordinator;
pub mod error;
pub mod manager;
pub mod model;
pub mod store;
pub mod team;
pub mod team_store;
pub mod tools;

pub use coordinator::{
    DEFAULT_COORDINATOR_PROMPT, DEFAULT_MAX_WORKERS, INTERNAL_WORKER_TOOLS, WORKER_ROLE,
    build_core_with_coordinator, build_scratchpad_path, build_worker_tool_registry,
    ensure_scratchpad, is_coordinator_enabled,
};

pub use error::TaskError;
pub use manager::{TaskManager, TaskPatch, UpdateOutcome};
pub use model::{ListId, Task, TaskId, TaskStatus, TeamFile, TeamMemberSpec, TeamName};
pub use store::{FileTaskStore, InMemoryTaskStore, TaskStore};
pub use team::{lead_agent_id_for, parse_agent_id, validate_team_name};
pub use team_store::{FileTeamStore, InMemoryTeamStore, TeamStore};
pub use tools::register_all as register_task_tools;
pub use tools::{
    TaskCreateTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool,
    TeamCreateTool, TeamDeleteTool, TodoRow, TodoWriteTool,
};
