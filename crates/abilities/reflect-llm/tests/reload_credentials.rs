//! v1.0 多 Provider 路由:credential 级 reload 集成测试。
//!
//! 验证 `ModelRegistry::upsert_credential` / `remove_credential` 在
//! reload 场景下的语义:加新 credential、删 credential、修改已有
//! credential(weight 变更) — 都不影响已有 `Arc<dyn ModelClient>` 引用,
//! 让 in-flight turn 不被打断。
//!
//! 1. `upsert_appends_new_label_to_existing_pool`
//! 2. `upsert_replaces_existing_label_in_place`
//! 3. `remove_credential_drops_label_and_clears_cooldown`
//! 4. `remove_all_credentials_leaves_empty_pool`

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

fn entry(label: &str, name: &'static str, weight: u32) -> PoolEntry {
    PoolEntry {
        client: stub(name),
        label: label.into(),
        weight,
    }
}

/// 加新 credential 到已有 pool:不影响已有 entries,新 entry 立即可被
/// `next_for` 选中。
#[tokio::test]
async fn upsert_appends_new_label_to_existing_pool() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1)],
        },
    );

    // reload: 用户加了一条 [[openai.credentials]] label = "personal"
    r.upsert_credential("openai", entry("personal", "openai", 2));

    let labels: Vec<String> = (0..6)
        .map(|_| r.next_for("openai/x", &[]).unwrap().label)
        .collect();
    // weight=1 (work) + weight=2 (personal) → 序列: w/p/p/w/p/p
    assert_eq!(
        labels,
        vec![
            "work", "personal", "personal", "work", "personal", "personal"
        ]
    );
}

/// 修改已有 credential (weight 变化):upsert 按 label 替换,新 weight
/// 立即生效。
#[tokio::test]
async fn upsert_replaces_existing_label_in_place() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1), entry("personal", "openai", 1)],
        },
    );

    // reload: 把 "work" 的 weight 改成 3。
    r.upsert_credential("openai", entry("work", "openai", 3));

    let labels: Vec<String> = (0..8)
        .map(|_| r.next_for("openai/x", &[]).unwrap().label)
        .collect();
    // weight=3 (work) + weight=1 (personal) → 序列: w/w/w/p/w/w/w/p
    assert_eq!(
        labels,
        vec![
            "work", "work", "work", "personal", "work", "work", "work", "personal"
        ]
    );
}

/// 删除 credential:label 从 pool 移除,该 label 的 cooldown 也清掉。
#[tokio::test]
async fn remove_credential_drops_label_and_clears_cooldown() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1), entry("personal", "openai", 1)],
        },
    );

    // 先把 "work" 标 cooldown,然后删除 → cooldown 跟着清。
    r.mark_cooldown(
        "openai",
        "work",
        Duration::from_secs(3600),
        CooldownReason::Auth,
    );
    // 拿一个 next_for 确保 cooldown 生效 (跳过 work,拿 personal)
    let nc = r.next_for("openai/x", &[]).unwrap();
    assert_eq!(nc.label, "personal", "work 被 cooldown 应跳过");

    r.remove_credential("openai", "work");

    // 删除后,pool 只剩 personal;再 next_for 应只拿 personal。
    let nc2 = r.next_for("openai/x", &[]).unwrap();
    assert_eq!(nc2.label, "personal");
}

/// 全部 credential 删完:pool 保留(空 entries),`next_for` 返回 `None`
/// 让 `model_call` 走 `ALL_CREDENTIALS_EXHAUSTED` 路径。
#[tokio::test]
async fn remove_all_credentials_leaves_empty_pool() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1)],
        },
    );

    r.remove_credential("openai", "work");

    assert!(
        r.next_for("openai/x", &[]).is_none(),
        "空 pool 应让 next_for 返回 None,触发 ALL_CREDENTIALS_EXHAUSTED"
    );
}

/// 既有 reload:`register_pool` 整池替换 vs `upsert_credential` 增量
/// upsert 两种方式都正确反映新 pool 内容。
#[tokio::test]
async fn register_pool_replaces_wholesale() {
    let r = ModelRegistry::new();
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1)],
        },
    );

    // 整段重写 — 模拟 config.toml 改了 base_url 触发的重建。
    r.register_pool(
        "openai",
        CredentialPool {
            entries: vec![entry("work", "openai", 1), entry("personal", "openai", 1)],
        },
    );

    let labels: Vec<String> = (0..4)
        .map(|_| r.next_for("openai/x", &[]).unwrap().label)
        .collect();
    // weight=1 + 1 → 交替
    assert_eq!(labels, vec!["work", "personal", "work", "personal"]);
}
