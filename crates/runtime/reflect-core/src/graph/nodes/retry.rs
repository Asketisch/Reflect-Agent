//! LLM 错误 → 重试 / failover 动作分类(v1.0 多 provider)。

use std::time::Duration;

use reflect_llm::LlmError;

/// v1.0 多 Provider 路由:把 LLM 错误分类为「下一步动作」决策。
/// 比 v0.x 的 `classify_retry` 多了 Failover / CooldownAndFailover 两支,
/// 由 `model_call` 据此切换 `ModelRegistry::next_for` 而非同 credential
/// 重试。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // 留作 Phase 3/4 扩展 / 单测用
pub(crate) enum RetryAction {
    /// 同 credential 立即重试(瞬时网络/5xx)。
    RetrySame { delay_ms: u64 },
    /// 切到下一个 credential,无 cooldown。
    Failover,
    /// 把当前 credential 标 cooldown + 切下一个。
    CooldownAndFailover { cooldown: Duration },
    /// 不可重试(`Auth` 自身也走 CooldownAndFailover,这条留给
    /// `Cancelled` / `ContextLengthExceeded` / 4xx 客户端错误)。
    GiveUp,
}

pub(crate) fn classify_action(
    e: &LlmError,
    default_cooldown_rate_limited: Duration,
) -> RetryAction {
    match e {
        LlmError::Auth => RetryAction::CooldownAndFailover {
            // Auth 不会自愈,1 小时冷却,期间 routing 切到别的 key。
            cooldown: Duration::from_secs(3600),
        },
        LlmError::RateLimited { retry_after_ms } => RetryAction::CooldownAndFailover {
            cooldown: Duration::from_millis(*retry_after_ms).max(default_cooldown_rate_limited),
        },
        LlmError::Overloaded { retry_after_ms } => RetryAction::CooldownAndFailover {
            cooldown: Duration::from_millis(*retry_after_ms),
        },
        LlmError::Provider { status, .. } if *status >= 500 => RetryAction::CooldownAndFailover {
            cooldown: Duration::from_secs(60),
        },
        LlmError::Http(_) => RetryAction::RetrySame { delay_ms: 1000 },
        LlmError::SseParse(_) | LlmError::Internal(_) => RetryAction::RetrySame { delay_ms: 500 },
        LlmError::Provider { .. } => RetryAction::GiveUp, // 任意未匹配的 4xx(包含 400..=499)
        LlmError::ContextLengthExceeded { .. }
        | LlmError::InvalidRequest { .. }
        | LlmError::Cancelled => RetryAction::GiveUp,
    }
}

#[allow(dead_code)]
pub(crate) fn classify_retry(e: &LlmError) -> (bool, u64) {
    match e {
        LlmError::Http(_) | LlmError::Provider { .. } => (true, 1000),
        LlmError::Overloaded { .. } => (true, 1000),
        LlmError::RateLimited { retry_after_ms } => (true, *retry_after_ms),
        LlmError::Auth
        | LlmError::InvalidRequest { .. }
        | LlmError::ContextLengthExceeded { .. }
        | LlmError::Cancelled
        | LlmError::SseParse(_)
        | LlmError::Internal(_) => (false, 0),
    }
}

/// 抽取 `LlmError` → `RoutingEvent.outcome` 字符串码。
pub(crate) fn outcome_code(e: &LlmError) -> &'static str {
    match e {
        LlmError::Auth => "auth",
        LlmError::RateLimited { .. } => "rate_limited",
        LlmError::Overloaded { .. } => "overloaded",
        LlmError::Provider { status, .. } if *status >= 500 => "provider_5xx",
        LlmError::Http(_) => "network",
        LlmError::SseParse(_) => "sse_parse",
        LlmError::Internal(_) => "internal",
        LlmError::Provider { .. } => "provider_4xx",
        LlmError::ContextLengthExceeded { .. } => "context_too_long",
        LlmError::InvalidRequest { .. } => "invalid_request",
        LlmError::Cancelled => "cancelled",
    }
}

pub(crate) fn error_code(e: &LlmError) -> &'static str {
    match e {
        LlmError::Auth => "AUTH_FAILED",
        LlmError::RateLimited { .. } => "RATE_LIMITED",
        LlmError::ContextLengthExceeded { .. } => "CONTEXT_TOO_LONG",
        LlmError::InvalidRequest { .. } => "INVALID_REQUEST",
        LlmError::Provider { .. } => "PROVIDER_ERROR",
        LlmError::Overloaded { .. } => "OVERLOADED",
        LlmError::Http(_) => "NETWORK_ERROR",
        LlmError::SseParse(_) => "SSE_PARSE_ERROR",
        LlmError::Cancelled => "CANCELLED",
        LlmError::Internal(_) => "INTERNAL_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_retry_matrix() {
        assert!(!classify_retry(&LlmError::Auth).0);
        assert!(
            classify_retry(&LlmError::RateLimited {
                retry_after_ms: 5000
            })
            .0
        );
        assert!(classify_retry(&LlmError::Overloaded { retry_after_ms: 0 }).0);
        assert!(classify_retry(&LlmError::Http("x".into())).0);
        assert!(
            classify_retry(&LlmError::Provider {
                status: 500,
                message: "x".into()
            })
            .0
        );
        assert!(!classify_retry(&LlmError::Cancelled).0);
        assert!(
            !classify_retry(&LlmError::InvalidRequest {
                message: "x".into()
            })
            .0
        );
    }

    #[test]
    fn error_code_covers_all_variants() {
        assert_eq!(error_code(&LlmError::Auth), "AUTH_FAILED");
        assert_eq!(error_code(&LlmError::Cancelled), "CANCELLED");
        assert_eq!(
            error_code(&LlmError::ContextLengthExceeded { used: 0, limit: 0 }),
            "CONTEXT_TOO_LONG"
        );
    }
}
