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
//! reflect-core — AgentThread、submission_loop,以及(M2+)4 节点 StateGraph。
//!
//! M1 提供驱动单轮 LLM 调用所需的最小能力:
//! - `AgentConfig` — 运行时配置
//! - `AgentThread` — 持有 submission 通道与全局事件分发任务
//! - `TurnHandle` — 每轮事件接收器(M2+ 沿用此契约)
//! - `submission_loop` — 处理 `Op` 变体并分派给单轮 runner
//!
//! M2 新增:4 节点 StateGraph、`ToolExecutionQueue`、多 turn、hooks,
//! 以及状态持久化。

pub mod agent_thread;
pub mod background_tasks;
pub mod config;
pub mod graph;
pub mod resume;
pub mod steering_queue;
pub mod subagent_registry;
pub mod submission_loop;
pub mod turn;
pub mod workspace;

pub use agent_thread::AgentThread;
pub use background_tasks::{
    BackgroundTask, BackgroundTaskQueue, BackgroundTaskStatus, spawn_background_stub,
};
pub use config::AgentConfig;
pub use steering_queue::{SteeringMessage, SteeringPriority, SteeringQueue};
pub use subagent_registry::SubagentRuntimeRegistry;
pub use submission_loop::NodeContext;
pub use turn::TurnHandle;
pub use workspace::detect_project_root;
