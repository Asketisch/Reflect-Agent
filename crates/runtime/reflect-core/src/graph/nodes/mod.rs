//! 4 节点图的节点实现(M2/M3)。
//!
//! 每个函数接收 `&mut AgentState` 与共享上下文(registry、hooks、tools 等),
//! 返回下一个要运行的节点;若 turn 结束则返回 `None`。
//!
//! 实现拆分到兄弟模块:
//! - [`pre_loop`] — 完整 turn 前置流水线(compact、memory、system prompt)。
//! - [`model_call`] — LLM 调用 + 流式 + retry/failover。
//! - [`tool_exec`] — 经 queue 分发 tool_use blocks。
//! - [`check_stop`] — 分发 `Stop` hook。
//! - [`nudge`] — progress-nudge / loop-guard 辅助 + `url_host`。
//! - [`retry`] — LLM 错误 → retry/failover 分类。
//!
//! 下方的 `pub use` 重新导出保留历史 `crate::graph::nodes::<item>` API
//! 形态(被 `graph::StateGraph` 与集成测试使用)。

mod check_stop;
mod model_call;
mod nudge;
mod pre_loop;
mod retry;
mod tool_exec;

pub use check_stop::check_stop;
pub use model_call::model_call;
pub use nudge::{maybe_inject_progress_nudge, url_host};
pub use pre_loop::pre_loop;
pub use tool_exec::tool_exec;
