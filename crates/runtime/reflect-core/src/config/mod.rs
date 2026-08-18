//! `AgentThread` 的运行时配置。
//!
//! 实现拆分到兄弟模块:
//! - [`agent`] — `AgentConfig` 结构体 + 全部方法 + `Debug` impl。
//! - [`m4`] — `M4Deps` 包 + `default_m4_deps` 测试辅助。
//! - [`env`] — 环境变量驱动的 resolver(compaction 触发、token 预算、
//!   最大迭代数)与相关常量。
//!
//! 下方的 `pub use` 重新导出保留历史 `crate::config::<item>` API
//! 形态(被 `reflect-exec`、`reflect-subagent`、`reflect-tui` 与集成测试使用)。

mod agent;
mod env;
mod m4;

pub use agent::AgentConfig;
pub use env::{
    AUTO_COMPACT_INPUT_TOKENS_ENV, DEFAULT_MAX_ITERATIONS, MAX_ITERATIONS_ENV, TOKEN_BUDGET_ENV,
    compactor_config_from_env_and_toml, max_iterations_from_env, token_budget_from_env,
    trigger_tokens_from_env,
};
pub use m4::{M4Deps, default_m4_deps};
