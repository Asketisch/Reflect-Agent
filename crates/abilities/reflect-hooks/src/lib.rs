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
//! `reflect-hooks` —— Hook trait、HookEngine、5 类事件、5 种决策、内置 hook。
//!
//! 协议参见 `docs/tools-and-hooks.md §4`。`PermissionMode` 从
//! `reflect-protocol` 再导出(其实际定义位于该 crate,以打破
//! `reflect-tools` ↔ `reflect-hooks` 的依赖循环)。

pub mod abort;
pub mod bash_classify;
pub mod builtins;
pub mod config;
pub mod decision;
pub mod engine;
pub mod event;
pub mod file_read_state;
pub mod hook;

pub use abort::HookAbortSignal;
pub use bash_classify::{BashCommandClass, classify_command};
pub use decision::{HookDecision, SystemMessage};
pub use engine::HookEngine;
pub use event::{HookContext, HookEvent, HookEventKind, StopReason};
pub use file_read_state::{DenyReason, FileReadStateTracker, ReadRecord, SharedFileReadState};
pub use hook::{Hook, HookError};

// 再导出 PermissionMode,便于把其视作 hook 协议一部分的用户使用
// (确实如此 —— 它出现在 `HookDecision::PermissionOverride` 中)。
pub use reflect_protocol::PermissionMode;
