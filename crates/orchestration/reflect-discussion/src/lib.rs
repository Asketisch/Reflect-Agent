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
//! `reflect-discussion` — 多 Agent 讨论编排器与消息总线(M9)。
//!
//! 讨论系统由 6 个模块组成,按依赖顺序自下而上:
//! 1. [`models`] — 核心数据类型(`DiscussionId` / `DiscussionMessage` / `DiscussionConfig` / `DiscussionResult`)
//! 2. [`message_bus`] — 进程内 `mpsc` 路由 + transcript 持久化
//! 3. [`comm_tools`] + [`tool`] — LLM 可见的三个通信工具(`send_message` / `read_messages` / `finish_discussion`)
//! 4. [`runtime`] + [`orchestrator`] — 顺序/并发调度循环 + 共识检测
//!
//! 讨论中的每个 Agent 通过 [`reflect_subagent::SubAgentFactory`] 生成(每角色一个
//! `AgentThread`),[`DiscussionOrchestrator`] 负责调度 + 共识检测 + transcript 持久化。
//!
//! 协议不暴露讨论状态(参见 `docs/protocol.md §342` 的 `EventMsg::Collab*` 段);状态
//! 完全在本 crate 内消化,唯一外部可见的是持久化到 JSONL 的
//! `reflect_protocol::RolloutRecord::DiscussionTranscript` 变体。
//!
//! 公开入口:`DiscussionOrchestrator` + `MessageBus` + `models::*` + `runtime::DiscussionRuntime`。
//! 通信工具以 `comm_tools::*` 暴露(advanced 用例);普通用户用 `tool::DiscussionToolSet::new`。

// PR-A 阶段先只暴露 models + message_bus;PR-B 再加 comm_tools / tool / runtime / orchestrator。
// v0.2.x 加 llm 模块接通 SubAgentFactory。
pub mod cli;
pub mod comm_tools;
pub mod llm;
pub mod message_bus;
pub mod models;
pub mod orchestrator;
pub mod runtime;
pub mod tool;

pub use cli::AgentSection;
pub use comm_tools::{FinishDiscussionTool, ReadMessagesTool, SendMessageTool};
pub use llm::{LlmContext, LlmError, build_context, prompt_for_closure};
pub use message_bus::{AgentMailbox, BusError, MessageBus};
pub use models::{
    AgentId, DiscussionConfig, DiscussionId, DiscussionMessage, DiscussionMode, DiscussionResult,
    JudgeVerdict, MessageId, MessageKind,
};
pub use orchestrator::{DiscussionOrchestrator, OrchestratorError, OrchestratorEvent};
pub use runtime::{DiscussionRuntime, RuntimeError};
pub use tool::DiscussionToolSet;
