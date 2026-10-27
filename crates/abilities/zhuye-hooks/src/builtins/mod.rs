//! 引擎自带的内置 hook。

pub mod langfuse_tracker;
pub mod plan_completion;
pub mod plan_mode_gate;
pub mod read_before_edit;
pub mod search_budget;
pub mod test_runner;
pub mod verification;

pub use langfuse_tracker::LangfuseTracker;
pub use plan_completion::PlanCompletionHook;
pub use plan_mode_gate::PlanModeGate;
pub use read_before_edit::{
    ReadBeforeEditConfig, ReadBeforeEditHook, WRITE_TOOLS, build_read_before_edit,
};
pub use search_budget::SearchBudgetHook;
pub use test_runner::TestRunnerHook;
pub use verification::VerificationHook;
