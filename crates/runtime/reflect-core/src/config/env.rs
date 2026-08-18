//! 基于环境变量的配置解析(compaction 触发、session token 预算、最大迭代数)。
//!
//! 每个 resolver 在调用时读取 env,以便测试在用例间切换;生产调用方
//! (`reflect-exec::bootstrap`)在启动时拍下快照。

use reflect_compact::CompactorConfig;

/// 覆盖默认 compaction 触发阈值的环境变量。
pub const AUTO_COMPACT_INPUT_TOKENS_ENV: &str = "REFLECT_AUTO_COMPACT_INPUT_TOKENS";

/// v1.2 P1-12:设置 session 级 token 硬预算的环境变量。当累计 session 用量
/// (input + output)达到该值时,turn 以 `TurnStatus::TokenBudgetExceeded` 结束。
/// 优先级:env `REFLECT_TOKEN_BUDGET` > TOML `[token_budget].session_total_tokens` > None。
pub const TOKEN_BUDGET_ENV: &str = "REFLECT_TOKEN_BUDGET";

/// v1.x:覆盖全局 agent-loop 迭代上限的环境变量。
/// 优先级:env `REFLECT_MAX_ITERATIONS` > TOML `active.max_iterations`
/// > [`DEFAULT_MAX_ITERATIONS`] (32)。
pub const MAX_ITERATIONS_ENV: &str = "REFLECT_MAX_ITERATIONS";

/// 当 env 与 TOML 都未配置时,每轮 `model_call` 迭代上限的默认值。镜像历史硬编码值。
pub const DEFAULT_MAX_ITERATIONS: u32 = 32;

/// 按优先级解析 session token 预算:env `REFLECT_TOKEN_BUDGET` > `toml_budget` > `None`。
///
/// 调用时读取 env,以便测试在用例间切换。生产调用方(`reflect-exec::bootstrap`)
/// 在启动时拍快照。与 [`trigger_tokens_from_env`](compaction)形态一致,但返回
/// `Option<u64>`,因为「无预算」也是合法默认值(仅靠 `max_iterations` 限制循环)。
pub fn token_budget_from_env(toml_budget: Option<u64>) -> Option<u64> {
    if let Some(env_val) = std::env::var(TOKEN_BUDGET_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(env_val);
    }
    toml_budget
}

/// 按优先级解析全局 agent-loop 迭代上限:env `REFLECT_MAX_ITERATIONS` >
/// `toml_max_iterations` > [`DEFAULT_MAX_ITERATIONS`] (32)。
///
/// 调用时读取 env,以便测试在用例间切换。生产调用方(`reflect-exec::bootstrap`)
/// 在启动时拍快照。与 [`token_budget_from_env`] 形态一致,总是返回具体的
/// `u32`(缺失时回退到文档化默认值,而非禁用上限 —— 无界 agent loop 永远不可取)。
pub fn max_iterations_from_env(toml_max_iterations: Option<u32>) -> u32 {
    if let Some(env_val) = std::env::var(MAX_ITERATIONS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        return env_val;
    }
    toml_max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS)
}

/// 默认触发阈值。调用时读取环境变量,以便测试在用例间切换;
/// 生产调用方(`reflect-exec::bootstrap_m5`)在启动时拍快照。
pub fn trigger_tokens_from_env() -> u32 {
    std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// 按优先级解析触发阈值,构造 `CompactorConfig`:env `REFLECT_AUTO_COMPACT_INPUT_TOKENS`
/// > TOML `trigger_tokens` > `CompactorConfig::default()` (= 10 000)。
///
/// `toml_trigger_tokens` 取自 `~/.reflect/config.toml` 中 `ReflectConfig.compact.trigger_tokens`。
/// 若该 section 缺失则传 `None`。`CompactorConfig::default()` 是其他可调参数
/// (`microcompact_ratio`、`keep_recent_*`、`target_ratio`、`summarize_after`)
/// 的单一事实源;此处仅覆盖 `trigger_tokens`。
///
/// 函数在调用时读取 env,以便测试在用例间翻转 `REFLECT_AUTO_COMPACT_INPUT_TOKENS`
/// (M7 parity:`std::env::set_var` 的竞态由测试套件中的 per-module env lock 串行化)。
pub fn compactor_config_from_env_and_toml(toml_trigger_tokens: Option<u32>) -> CompactorConfig {
    let mut cfg = CompactorConfig::default();
    if let Some(env_val) = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        cfg.trigger_tokens = env_val;
    } else if let Some(toml_val) = toml_trigger_tokens {
        cfg.trigger_tokens = toml_val;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// `std::env::set_var` 在 Rust 2024 中是 `unsafe`(与并发 `env::var`
    /// 读取非线程安全)。本模块所有测试经这把锁串行,避免并行测试
    /// 执行期间的数据竞争。
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn trigger_tokens_from_env_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        assert_eq!(trigger_tokens_from_env(), 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn trigger_tokens_from_env_override() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "12345");
        }
        assert_eq!(trigger_tokens_from_env(), 12_345);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    #[test]
    fn trigger_tokens_from_env_malformed_returns_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "not-a-number");
        }
        assert_eq!(trigger_tokens_from_env(), 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    // ── M8 P0b:压缩器配置(环境变量 + TOML)──────────────────

    #[test]
    fn compactor_config_default_when_no_env_no_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        let cfg = compactor_config_from_env_and_toml(None);
        assert_eq!(cfg.trigger_tokens, 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn compactor_config_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        let cfg = compactor_config_from_env_and_toml(Some(5_000));
        assert_eq!(cfg.trigger_tokens, 5_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn compactor_config_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "200");
        }
        let cfg = compactor_config_from_env_and_toml(Some(99_999));
        assert_eq!(cfg.trigger_tokens, 200, "env must override TOML");
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    #[test]
    fn compactor_config_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "not-a-number");
        }
        let cfg = compactor_config_from_env_and_toml(Some(7_777));
        assert_eq!(
            cfg.trigger_tokens, 7_777,
            "malformed env should fall through to TOML"
        );
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    // ── v1.2 P1-12:token 预算 ─────────────────────────────────────

    /// `token_budget_from_env`:无 env 无 toml → `None`(默认不设预算)。
    #[test]
    fn token_budget_from_env_default_none() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::remove_var(TOKEN_BUDGET_ENV);
        }
        assert_eq!(token_budget_from_env(None), None);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        }
    }

    /// `token_budget_from_env`:toml 有值、env 无 → 用 toml 值。
    #[test]
    fn token_budget_from_env_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::remove_var(TOKEN_BUDGET_ENV);
        }
        assert_eq!(token_budget_from_env(Some(500_000)), Some(500_000));
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        }
    }

    /// `token_budget_from_env`:env 优先于 toml。
    #[test]
    fn token_budget_from_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::set_var(TOKEN_BUDGET_ENV, "999");
        }
        assert_eq!(
            token_budget_from_env(Some(500_000)),
            Some(999),
            "env must override TOML"
        );
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(TOKEN_BUDGET_ENV);
            }
        }
    }

    /// `token_budget_from_env`:malformed env → 回退 toml(不报错)。
    #[test]
    fn token_budget_from_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::set_var(TOKEN_BUDGET_ENV, "not-a-number");
        }
        assert_eq!(token_budget_from_env(Some(7_777)), Some(7_777));
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(TOKEN_BUDGET_ENV);
            }
        }
    }

    // ── v1.x:最大迭代数(env 与 TOML 合成)─────────────────────

    /// `max_iterations_from_env`:无 env 无 toml → 默认 32。
    #[test]
    fn max_iterations_from_env_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::remove_var(MAX_ITERATIONS_ENV);
        }
        assert_eq!(max_iterations_from_env(None), DEFAULT_MAX_ITERATIONS);
        assert_eq!(max_iterations_from_env(None), 32);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        }
    }

    /// `max_iterations_from_env`:toml 有值、env 无 → 用 toml 值。
    #[test]
    fn max_iterations_from_env_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::remove_var(MAX_ITERATIONS_ENV);
        }
        assert_eq!(max_iterations_from_env(Some(8)), 8);
        assert_eq!(max_iterations_from_env(Some(100)), 100);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        }
    }

    /// `max_iterations_from_env`:env 优先于 toml。
    #[test]
    fn max_iterations_from_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::set_var(MAX_ITERATIONS_ENV, "7");
        }
        assert_eq!(max_iterations_from_env(Some(99)), 7);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(MAX_ITERATIONS_ENV);
            }
        }
    }

    /// `max_iterations_from_env`:malformed env → 回退 toml(不报错)。
    #[test]
    fn max_iterations_from_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::set_var(MAX_ITERATIONS_ENV, "not-a-number");
        }
        assert_eq!(max_iterations_from_env(Some(42)), 42);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(MAX_ITERATIONS_ENV);
            }
        }
    }
}
