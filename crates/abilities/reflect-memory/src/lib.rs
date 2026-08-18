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
//! reflect-memory —— 三 scope(project / user / session)持久 memory。
//!
//! Reflect 路线图的 M4。移植自 Reflect `agent_memory.py`。
//!
//! 三种 scope:
//!
//! - [`MemoryScope::Project`] —— `{workspace}/.reflect/agent-memory/{agent_type}/MEMORY.md`。
//!   可入 VCS 共享;跨会话保留;用户择机提交。
//! - [`MemoryScope::User`] —— `~/.reflect/agent-memory/{agent_type}/MEMORY.md`。
//!   跨项目、本机范围。
//! - [`MemoryScope::Session`] —— 仅内存。M5 将经 JSONL rollout 持久化;
//!   目前存放在 [`InMemoryStore`] 内的 `HashMap` 中。
//!
//! 注入 memory 的字符上限为
//! [`MAX_MEMORY_INJECT_CHARS`](8000,与 Reflect 一致)。

pub mod model;
pub mod scope;
pub mod store;

pub use model::{MemoryError, MemoryScope};
pub use scope::{MAX_MEMORY_INJECT_CHARS, resolve_path};
pub use store::{FileMemoryStore, InMemoryStore, MemoryStore, truncate_for_injection};
