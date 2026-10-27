//! 单 credential 冷却状态;存活于 `ModelRegistry` 内部。
//!
//! 触发时机:当 `client.stream` 返回 `LlmError::RateLimited` /
//! `Auth` / `Overloaded` / `5xx` 时由 `model_call` 调
//! `ModelRegistry::mark_cooldown`,记录该 credential 应被跳过的截止
//! 时间。`next_for` 在派位前过滤掉 `until > now` 的 entry,实现
//! "冷却期不参与轮询"。
//!
//! 半开恢复:不显式 probe。`until <= now` 时 entry 自动重新可用;
//! 再次失败由 `mark_cooldown` 应用指数退避(Phase 4 引入):
//! credential 仍在 cooldown 时再次失败,新 `until = now + max(2 × remaining,
//! requested)`,cap 1h。`previous_until` 字段供审计 / tracing 用。
//!
//! `Auth` 不需 special-case:caller 给 3600s,1h 后旧 entry
//! `is_active == false`,自然走「重新 1h」路径。

use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownEntry {
    pub until: Instant,
    pub reason: CooldownReason,
    /// 本次 cooldown 之前的 until(用于审计 / tracing);
    /// `None` = 该 credential 首次进 cooldown。
    /// **不**进 serde / wire —— `Instant` 不可序列化,本 struct 也不
    /// derive `Serialize` / `Deserialize`,仅存活于 `ModelRegistry` 内部。
    pub previous_until: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooldownReason {
    /// 429 + `Retry-After`(毫秒)。
    RateLimited { retry_after_ms: u64 },
    /// 401:key 死了,长 cooldown。
    Auth,
    /// 529 (Anthropic) 或类似 overloaded 信号。
    Overloaded,
    /// 5xx:provider 端故障,中等 cooldown。
    Provider5xx { status: u16 },
    /// 半开探测标记(v1.0 暂未启用,留 Phase 4 指数退避用)。
    HalfOpenProbe,
    /// v1.x 功能 7:token plan 配额耗尽。cooldown 到窗口结束
    /// (`window_ends_secs`),届时 `used_tokens` 归零、credential 自动可用。
    /// 由 `model_call` 在 `QuotaTracker::is_exhausted` 为真时触发,
    /// `next_for` 顺位切到下一个 credential(plan B)。
    QuotaExhausted { window_ends_secs: u64 },
}

impl CooldownEntry {
    pub fn new(until: Instant, reason: CooldownReason) -> Self {
        Self {
            until,
            reason,
            previous_until: None,
        }
    }

    /// `now < until` 即认为仍处于冷却期。
    pub fn is_active(&self, now: Instant) -> bool {
        now < self.until
    }
}

/// Phase 4:把 mark_cooldown 内部的指数退避决策抽成纯函数,便于单测
/// 覆盖边界(remaining=0、remaining × 2 > cap、requested > remaining 等)。
///
/// `prev_remaining` = 上次 cooldown 剩余时间(now - prev.until 的负值取正);
/// `requested` = caller 本次请求的 cooldown duration。
///
/// 返新 cooldown duration,`now + duration` 即新 `until`。
pub fn compute_backoff(
    prev_remaining: std::time::Duration,
    requested: std::time::Duration,
) -> std::time::Duration {
    const CAP: std::time::Duration = std::time::Duration::from_secs(3600);
    (prev_remaining.saturating_mul(2)).max(requested).min(CAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn compute_backoff_doubles_remaining_when_active() {
        let prev = Duration::from_secs(60);
        let requested = Duration::from_secs(30);
        assert_eq!(compute_backoff(prev, requested), Duration::from_secs(120));
    }

    #[test]
    fn compute_backoff_uses_max_when_requested_larger() {
        let prev = Duration::from_secs(10);
        let requested = Duration::from_secs(300);
        // max(20, 300) = 300
        assert_eq!(compute_backoff(prev, requested), Duration::from_secs(300));
    }

    #[test]
    fn compute_backoff_caps_at_one_hour() {
        let prev = Duration::from_secs(3600); // remaining 接近 1h
        let requested = Duration::from_secs(0);
        // 3600 * 2 = 7200,cap 3600
        assert_eq!(compute_backoff(prev, requested), Duration::from_secs(3600));
    }

    #[test]
    fn compute_backoff_zero_remaining_uses_requested() {
        assert_eq!(
            compute_backoff(Duration::ZERO, Duration::from_secs(45)),
            Duration::from_secs(45)
        );
    }
}
