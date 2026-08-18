//! v1.0 多 Provider 路由:wiremock 端到端 failover 集成测试。
//!
//! 这些测试不依赖 `reflect-core` 的 `model_call` 循环,而是直接验证
//! `ModelRegistry` + `next_for` + `mark_cooldown` 的池轮询语义。生产路径
//! 在 `reflect-core::graph::nodes::model_call` 已经覆盖(`single_turn` 测试),
//! 这里的 4 个用例专门验证 registry 层的 round-robin / cooldown 切换:
//!
//! 1. `rate_limited_fails_over_to_next_credential` — 429 触发 cooldown + 切
//! 2. `auth_fails_over_with_long_cooldown` — 401 长 cooldown + 切
//! 3. `all_credentials_exhausted_returns_none` — 3 个全不可用
//! 4. `clear_cooldown_re_enables_credential` — 清 cooldown 后恢复
//!
//! 注意:这些测试不跑 `client.stream`(那需要完整的 wiremock SSE),
//! 只验证 registry 层的调度逻辑。

use reflect_llm::{
    CooldownReason, CredentialPool, LlmError, ModelClient, ModelRegistry, PoolEntry,
};
use std::time::Duration;

struct StubClient {
    name: &'static str,
}
#[async_trait::async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        self.name
    }
    async fn stream(
        &self,
        _: reflect_llm::ChatRequest,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<reflect_llm::ChatEvent, LlmError>> + Send>,
        >,
        LlmError,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }
}

fn stub(name: &'static str) -> std::sync::Arc<dyn ModelClient> {
    std::sync::Arc::new(StubClient { name })
}

fn entry(label: &str, name: &'static str) -> PoolEntry {
    PoolEntry {
        client: stub(name),
        label: label.into(),
        weight: 1,
    }
}

/// RateLimited 触发 cooldown + 切下一个 credential。
#[tokio::test]
async fn rate_limited_fails_over_to_next_credential() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai"), entry("personal", "openai")],
        },
    );

    // 模拟"work 抛 RateLimited":registry 看到错误,业务侧应把它
    // 标 cooldown 并切到 personal。验证:第一次 work 失败 + 标 cooldown
    // → 第二次 next_for 拿 personal。
    let nc1 = r.next_for("openai/x", &[]).expect("work available");
    assert_eq!(nc1.label, "work");

    r.mark_cooldown(
        "openai",
        "work",
        Duration::from_millis(60_000),
        CooldownReason::RateLimited {
            retry_after_ms: 60_000,
        },
    );

    let nc2 = r.next_for("openai/x", &[]).expect("personal available");
    assert_eq!(nc2.label, "personal", "work 应被 cooldown 跳过");

    // cooldown 期间一直跳过 work
    for _ in 0..3 {
        let nc = r.next_for("openai/x", &[]).unwrap();
        assert_eq!(nc.label, "personal");
    }
}

/// Auth 触发 3600s 长 cooldown。
#[tokio::test]
async fn auth_fails_over_with_long_cooldown() {
    let r = ModelRegistry::new();
    r.register_pool(
        "anthropic",
        CredentialPool {
            entries: vec![entry("work", "anthropic"), entry("personal", "anthropic")],
        },
    );

    r.mark_cooldown(
        "anthropic",
        "work",
        Duration::from_secs(3600),
        CooldownReason::Auth,
    );

    // 多次 next_for 都应拿 personal(work cooldown 3600s)
    for _ in 0..5 {
        let nc = r
            .next_for("anthropic/x", &[])
            .expect("at least one healthy");
        assert_eq!(nc.label, "personal");
    }
}

/// 全部 credential 都在 cooldown / exclude → next_for 返 None。
#[tokio::test]
async fn all_credentials_exhausted_returns_none() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![
                entry("a", "openai"),
                entry("b", "openai"),
                entry("c", "openai"),
            ],
        },
    );
    // 全部标 cooldown
    for label in ["a", "b", "c"] {
        r.mark_cooldown(
            "openai",
            label,
            Duration::from_secs(60),
            CooldownReason::RateLimited {
                retry_after_ms: 60_000,
            },
        );
    }
    let nc = r.next_for("openai/x", &[]);
    assert!(nc.is_none(), "全部 cooldown 应返 None");
}

/// clear_cooldown 重新启用 credential。
#[tokio::test]
async fn clear_cooldown_re_enables_credential() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai"), entry("personal", "openai")],
        },
    );

    r.mark_cooldown(
        "openai",
        "work",
        Duration::from_secs(3600),
        CooldownReason::Auth,
    );
    // 只能拿 personal
    let nc = r.next_for("openai/x", &[]).unwrap();
    assert_eq!(nc.label, "personal");

    r.clear_cooldown("openai", "work");

    // 现在两个都可用,跑 2 次 round-robin 应该 work / personal 至少出现一次
    let mut saw_work = false;
    for _ in 0..4 {
        if r.next_for("openai/x", &[]).unwrap().label == "work" {
            saw_work = true;
            break;
        }
    }
    assert!(saw_work, "clear_cooldown 后 work 应在轮询里");
}
