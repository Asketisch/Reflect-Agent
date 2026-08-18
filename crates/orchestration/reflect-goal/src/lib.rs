//! Reflect Goal —— 目标模式(参考 zcode `/goal` 设计)。
//!
//! 让 agent 围绕目标**持续迭代**:每轮 turn 结束自动校验,未完成则继续。
//!
//! 校验 = **LLM 自校验为主 + 可选命令**(`update_goal` 自判定 + `VerificationHook` 命令式)。
//!
//! 状态机设计参考 `thread_goals.status`:`active / paused / blocked /
//! budget_limited / complete`,含 **3-strike blocked 规则**(同一
//! blocker 连续 3 个 goal turn 才允许放弃)+ token budget 软停。
//!
//! 续作机制复用 reflect-core 的 `steering_queue`(turn 结束若仍 Active,
//! controller 生成 continuation prompt 推入队列,下个 turn 自动开),不改
//! agent loop —— 轮次续作机制。
//!
//! 模块:
//! - [`state`] —— 状态机 + verdict + turn 记录。
//! - [`verifier`] —— LLM 自校验 + 命令校验。
//! - [`controller`] —— 每轮编排核心(`on_turn_end`)。

pub mod controller;
pub mod state;
pub mod verifier;

pub use controller::{GoalController, TurnResult, continuation_to_items};
pub use state::{
    BLOCKED_THRESHOLD, DEFAULT_TOKEN_BUDGET, GoalState, GoalStatus, GoalTurnRecord, GoalVerdict,
};
pub use verifier::{VerifyError, run_verify_command, verify};
