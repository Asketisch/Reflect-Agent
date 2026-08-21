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
//! `reflect-subagent` —— Tool-per-Agent 形式的子代理 factory 与 session forking。
//!
//! 每个子代理以 `call_<role>` 名称的工具形式暴露给父级 LLM。
//! 调用时,factory spawn 一个全新 `AgentThread`(in_flight + 1),跑一次
//! user-input turn,返回抽取出的结果。`SpawnedChild` 析构或在终态
//! `collect_result` 时自动释放槽位(in-flight 语义,而非总 spawn 数)。
//! 并发上限为 `MAX_IN_FLIGHT = 16`;同时超过该数量的 spawn 会返回
//! `SubAgentError::MaxDepthExceeded`。
//!
//! `Session::fork(branch_name)` 是一个薄封装:分配子 `ThreadId`,
//! 并向父级 recorder 写入 `RolloutRecord::Fork`,使父子 session 的关系在
//! 恢复时可查询。

pub mod data_transfer;
pub mod error;
pub mod factory;
pub mod loader;
pub mod spec;
pub mod tools;
pub mod worker_registry;

pub use data_transfer::{DataTransferConfig, ResultExtractor};
pub use error::SubAgentError;
pub use factory::{SpawnedChild, SubAgentFactory};
pub use loader::{
    LoadError, load_subagents_dir, merge_by_priority, parse_subagent_md, parse_subagent_str,
};
pub use spec::SubAgentSpec;
pub use tools::CallSubAgentTool;
pub use worker_registry::{INTERNAL_WORKER_TOOLS, build_worker_tool_registry};

/// 子代理最大并发 in-flight 上限(同一父 session 同时存活的 spawn 数)。
///
/// **注意**:这是 in-flight 计数,不是总 spawn 数。`SpawnedChild` 析构
/// 或 `collect_result` 完成时会自动释放槽位,确保长 session 不会累积
/// 到上限。`MAX_DEPTH` 名称保留向后兼容(已 export 给其它 crate)。
pub const MAX_DEPTH: u8 = 16;
