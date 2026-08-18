//! `ModelRegistry` — name → `CredentialPool` 多 credential 池。
//!
//! v1.0 多 Provider 路由改造:从 `HashMap<String, Arc<dyn ModelClient>>`
//! 单值映射升级为 `HashMap<String, CredentialPool>` 池化结构。同 provider
//! 可持 N 个 credential(同 provider 不同 API key),通过 `next_for` 做
//! weighted round-robin + cooldown 过滤 + exclude 过滤的派位决策。
//!
//! ## 锁设计
//!
//! - `pools` / `cooldowns` 用 `parking_lot::RwLock`(高读低写);
//! - `cursors` 用 `RwLock<HashMap<String, Arc<AtomicUsize>>>`:
//!   `next_for` 读锁取出 `Arc<AtomicUsize>`,后续 `fetch_add(1, Relaxed)`
//!   完全 lock-free,避免在 LLM 热路径上持锁。
//!
//! ## 兼容性
//!
//! `get` / `resolve` / `list` 行为对外保持:`get("anthropic")` 仍返回首个
//! available client(无 cooldown 时的池首)。Phase 4 移除旧
//! `register(name, client)` API —— v1.0.0-rc1 之前一直保留,
//! rc1 落地后所有 caller 已迁到 `register_pool` / `upsert_credential`。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::client::ModelClient;
use crate::cooldown::{CooldownEntry, CooldownReason, compute_backoff};

pub type SharedModelRegistry = Arc<ModelRegistry>;

/// 共享的 credential 池,每个 provider 持有一份。
pub struct ModelRegistry {
    /// provider 名 → 该 provider 下的 credential 池(顺序固定,不重排)。
    pools: RwLock<HashMap<String, CredentialPool>>,
    /// provider → label → cooldown 状态(双层 map)。
    cooldowns: RwLock<HashMap<String, HashMap<String, CooldownEntry>>>,
    /// provider → weighted round-robin 游标(`Arc<AtomicUsize>` 允许
    /// 读锁外 fetch_add)。
    cursors: RwLock<HashMap<String, Arc<AtomicUsize>>>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
            cooldowns: RwLock::new(HashMap::new()),
            cursors: RwLock::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pools = self.pools.read();
        let count = pools.len();
        let total: usize = pools.values().map(|p| p.entries.len()).sum();
        f.debug_struct("ModelRegistry")
            .field("provider_count", &count)
            .field("credential_count", &total)
            .finish_non_exhaustive()
    }
}

/// 单 provider 下的 credential 列表。
#[derive(Default)]
pub struct CredentialPool {
    pub entries: Vec<PoolEntry>,
}

/// 单 credential 记录:client + label + weight。
#[derive(Clone)]
pub struct PoolEntry {
    pub client: Arc<dyn ModelClient>,
    pub label: String,
    pub weight: u32,
}

/// `next_for` 返回值:带 label 诊断信息的 client。
#[derive(Clone)]
pub struct NextClient {
    pub client: Arc<dyn ModelClient>,
    pub label: String,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册完整的 credential pool。`register_pool` 是幂等的:同 provider
    /// 再调一次会整池替换,适用于 `apply_to_registry` 整段重写场景。
    ///
    /// 同时确保游标存在(避免 `next_for` 第一次走的锁升级)。
    pub fn register_pool(&self, name: impl Into<String>, pool: CredentialPool) {
        let name = name.into();
        self.pools.write().insert(name.clone(), pool);
        self.cursors
            .write()
            .entry(name)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)));
    }

    /// 整 provider 段移除(给 reload 用来整段重写)。同时清掉 cooldown
    /// 与游标。
    pub fn unregister(&self, name: &str) {
        self.pools.write().remove(name);
        self.cooldowns.write().remove(name);
        self.cursors.write().remove(name);
    }

    /// 在已有 pool 上 upsert 单 credential:`label` 存在则替换,否则追加。
    /// 适用于 `handle_reload` 增量 diff 场景。
    ///
    /// 调用前需保证 `name` 已有 pool(否则会插入空 entries 的 pool,
    /// 后续 `next_for` 返回 `None`)。若需要新建,先调 `register_pool`。
    pub fn upsert_credential(&self, name: &str, entry: PoolEntry) {
        let mut pools = self.pools.write();
        let pool = pools.entry(name.to_string()).or_default();
        if let Some(existing) = pool.entries.iter_mut().find(|e| e.label == entry.label) {
            *existing = entry;
        } else {
            pool.entries.push(entry);
        }
    }

    /// 按 label 删除单 credential。pool 变空后**保留**空 pool(让
    /// `handle_reload` 知道该 provider 仍存在)。
    pub fn remove_credential(&self, name: &str, label: &str) {
        let mut pools = self.pools.write();
        if let Some(pool) = pools.get_mut(name) {
            pool.entries.retain(|e| e.label != label);
        }
        if let Some(cd_map) = self.cooldowns.write().get_mut(name) {
            cd_map.remove(label);
        }
    }

    /// 旧 API:按 provider 名取首个 available client(无 cooldown 时的池首)。
    pub fn get(&self, name: &str) -> Option<Arc<dyn ModelClient>> {
        self.next_for(name, &[]).map(|nc| nc.client)
    }

    /// 旧 API:解析 `"provider/model"` spec 取首个 available client。
    /// `model_call` 等业务方仍可继续调用,Phase 2 改为 `next_for`。
    pub fn resolve(&self, spec: &str) -> Option<Arc<dyn ModelClient>> {
        self.next_for(spec, &[]).map(|nc| nc.client)
    }

    /// 解析 `"provider/model"` spec 的 model 部分(纯字符串,无锁)。
    pub fn model_name<'a>(spec: &'a str) -> &'a str {
        spec.split_once('/').map(|(_, m)| m).unwrap_or(spec)
    }

    /// 列出已注册的所有 provider 名(按字典序)。
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<_> = self.pools.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// v1.0 多 Provider 路由核心:从 `spec` 对应 provider 的 pool 里
    /// 派位出下一个可用的 client。
    ///
    /// 步骤:
    /// 1. 取 pool
    /// 2. 过滤掉仍在 cooldown 的 entry(`until > now`)
    /// 3. 过滤掉 `exclude` 列表里 `Arc::ptr_eq` 命中的 entry
    /// 4. 按 weight 加权 round-robin 选下标
    /// 5. `fetch_add(1, Relaxed)` 推进游标(锁外完成)
    ///
    /// 返回 `None` 表示该 provider 下所有 credential 都不可用。
    pub fn next_for(&self, spec: &str, exclude: &[Arc<dyn ModelClient>]) -> Option<NextClient> {
        let provider = spec.split_once('/').map(|(p, _)| p).unwrap_or(spec);
        let pools = self.pools.read();
        let pool = pools.get(provider)?;
        if pool.entries.is_empty() {
            return None;
        }
        let cooldowns = self.cooldowns.read();
        let provider_cooldowns = cooldowns.get(provider);
        let now = Instant::now();

        // 过滤:不在 cooldown、不在 exclude
        let available: Vec<usize> = pool
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if let Some(cd_map) = provider_cooldowns {
                    if let Some(cd) = cd_map.get(&e.label) {
                        if cd.is_active(now) {
                            return false;
                        }
                    }
                }
                !exclude.iter().any(|ex| Arc::ptr_eq(ex, &e.client))
            })
            .map(|(i, _)| i)
            .collect();

        if available.is_empty() {
            return None;
        }

        // 计算总权重(忽略 weight=0 的 entry,但上面已过滤掉 cooldown,
        // weight=0 entry 也应在过滤后被排除 —— 额外检查)
        let total_weight: u32 = available
            .iter()
            .map(|&i| pool.entries[i].weight)
            .filter(|&w| w > 0)
            .sum();
        if total_weight == 0 {
            return None;
        }

        // 取游标(锁外 fetch_add)
        let cursor = {
            let cursors_read = self.cursors.read();
            cursors_read.get(provider).cloned()
        };
        let cursor = match cursor {
            Some(c) => c,
            None => {
                // 旧 register 路径没建游标,补建一个。
                let mut cursors = self.cursors.write();
                cursors
                    .entry(provider.to_string())
                    .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
                    .clone()
            }
        };

        let pos = cursor.fetch_add(1, Ordering::Relaxed) as u32;
        let target = pos % total_weight;

        // 在 available 里按 weight 累加,找 target 落点
        let mut acc = 0u32;
        for &i in &available {
            let w = pool.entries[i].weight;
            if w == 0 {
                continue;
            }
            acc += w;
            if target < acc {
                let entry = &pool.entries[i];
                return Some(NextClient {
                    client: entry.client.clone(),
                    label: entry.label.clone(),
                });
            }
        }
        // 理论不可达(累加必 ≥ total_weight ≥ target+1);兜底返回最后
        let last_idx = *available.last().expect("non-empty available");
        let entry = &pool.entries[last_idx];
        Some(NextClient {
            client: entry.client.clone(),
            label: entry.label.clone(),
        })
    }

    /// 把 provider 下的某 credential 标记为 cooldown。
    ///
    /// Phase 4 指数退避(取代 v1.0 的「无条件覆盖」):
    /// - 旧 cooldown entry 仍在 active(`now < prev.until`):
    ///   新 `until = now + max(2 × remaining, requested)`,cap 1h。
    /// - 旧 entry 已过期(上轮 cooldown 自然到期):按新 `duration` 重置,
    ///   `previous_until` 留作审计。
    /// - 首次进 cooldown:`previous_until = None`,`until = now + duration`。
    ///
    /// 持写锁时间 < 1ms(pure Instant + saturating_duration_since)。
    pub fn mark_cooldown(
        &self,
        provider: &str,
        label: &str,
        duration: Duration,
        reason: CooldownReason,
    ) {
        let now = Instant::now();
        let mut cd_map = self.cooldowns.write();
        let inner = cd_map.entry(provider.to_string()).or_default();
        let (new_until, previous_until) = match inner.get(label) {
            Some(prev) if prev.is_active(now) => {
                let remaining = prev.until.saturating_duration_since(now);
                let backoff = compute_backoff(remaining, duration);
                (now + backoff, Some(prev.until))
            }
            Some(prev) => (now + duration, Some(prev.until)),
            None => (now + duration, None),
        };
        inner.insert(
            label.to_string(),
            CooldownEntry {
                until: new_until,
                reason,
                previous_until,
            },
        );
    }

    /// 清除 cooldown(成功调用后由 `model_call` 调)。
    pub fn clear_cooldown(&self, provider: &str, label: &str) {
        if let Some(cd_map) = self.cooldowns.write().get_mut(provider) {
            cd_map.remove(label);
        }
    }

    /// 列出某 provider 下当前不在 cooldown 的 client,供 TUI `/status` /
    /// `healthy_clients` 等只读接口用。
    pub fn healthy_clients(&self, spec: &str) -> Vec<NextClient> {
        let provider = spec.split_once('/').map(|(p, _)| p).unwrap_or(spec);
        let pools = self.pools.read();
        let Some(pool) = pools.get(provider) else {
            return Vec::new();
        };
        let cooldowns = self.cooldowns.read();
        let provider_cooldowns = cooldowns.get(provider);
        let now = Instant::now();
        pool.entries
            .iter()
            .filter(|e| {
                if let Some(cd_map) = provider_cooldowns {
                    if let Some(cd) = cd_map.get(&e.label) {
                        if cd.is_active(now) {
                            return false;
                        }
                    }
                }
                true
            })
            .map(|e| NextClient {
                client: e.client.clone(),
                label: e.label.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmError;
    use crate::event::ChatEvent;
    use async_trait::async_trait;
    use futures::stream;
    use std::pin::Pin;

    struct Stub(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl ModelClient for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        async fn stream(
            &self,
            _: crate::request::ChatRequest,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<
            Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
            LlmError,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::pin(stream::empty()))
        }
    }

    fn stub() -> Arc<Stub> {
        Arc::new(Stub(Arc::new(std::sync::atomic::AtomicUsize::new(0))))
    }
    fn as_client(s: Arc<Stub>) -> Arc<dyn ModelClient> {
        s
    }

    #[test]
    fn model_name_strips_provider() {
        assert_eq!(ModelRegistry::model_name("openai/gpt-4o"), "gpt-4o");
        assert_eq!(ModelRegistry::model_name("claude-3"), "claude-3");
    }

    // ── v1.0 多 Provider 路由:CredentialPool 测试 ──────────────────────

    fn entry(label: &str, weight: u32) -> PoolEntry {
        PoolEntry {
            client: as_client(stub()),
            label: label.to_string(),
            weight,
        }
    }

    fn pool(entries: Vec<PoolEntry>) -> CredentialPool {
        CredentialPool { entries }
    }

    #[test]
    fn register_pool_creates_single_credential_pool() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        let nc = r.next_for("openai/gpt-4o", &[]).expect("next_for");
        assert_eq!(nc.label, "work");
        assert_eq!(nc.client.name(), "stub");
    }

    #[test]
    fn next_for_round_robins_through_weighted_pool() {
        let r = ModelRegistry::new();
        r.register_pool(
            "openai",
            pool(vec![
                entry("work", 1),
                entry("personal", 1),
                entry("backup", 1),
            ]),
        );
        let labels: Vec<String> = (0..6)
            .map(|_| r.next_for("openai/x", &[]).unwrap().label)
            .collect();
        // 3 entries weight=1:顺序应稳定循环 work → personal → backup → ...
        assert_eq!(
            labels,
            vec!["work", "personal", "backup", "work", "personal", "backup"]
        );
    }

    #[test]
    fn next_for_weighted_distribution() {
        // work weight=2, personal weight=1 → 6 次里 work 占 4 次,personal 2 次
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 2), entry("personal", 1)]));
        let mut work = 0;
        let mut personal = 0;
        for _ in 0..6 {
            match r.next_for("openai/x", &[]).unwrap().label.as_str() {
                "work" => work += 1,
                "personal" => personal += 1,
                _ => panic!("unexpected label"),
            }
        }
        assert_eq!(work, 4, "weight=2 应占 4/6");
        assert_eq!(personal, 2, "weight=1 应占 2/6");
    }

    #[test]
    fn next_for_excludes_listed_clients() {
        let r = ModelRegistry::new();
        let work_client = as_client(stub());
        let personal_client = as_client(stub());
        r.register_pool(
            "openai",
            pool(vec![
                PoolEntry {
                    client: work_client.clone(),
                    label: "work".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: personal_client.clone(),
                    label: "personal".into(),
                    weight: 1,
                },
            ]),
        );
        // exclude work → 只剩 personal
        let nc = r.next_for("openai/x", &[work_client]).unwrap();
        assert_eq!(nc.label, "personal");
    }

    #[test]
    fn next_for_returns_none_when_all_excluded() {
        let r = ModelRegistry::new();
        let c1: Arc<dyn ModelClient> = as_client(stub());
        r.register_pool(
            "openai",
            pool(vec![PoolEntry {
                client: c1.clone(),
                label: "work".into(),
                weight: 1,
            }]),
        );
        // exclude 唯一的 entry → next_for 应返 None
        let nc = r.next_for("openai/x", &[c1]);
        assert!(nc.is_none());
    }

    #[test]
    fn next_for_skips_cooldown_entries() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1), entry("personal", 1)]));
        // work 进 cooldown 1 小时
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(3600),
            CooldownReason::Auth,
        );
        // 取 4 次,每次都应是 personal(work 被跳)
        for _ in 0..4 {
            let nc = r.next_for("openai/x", &[]).unwrap();
            assert_eq!(nc.label, "personal", "work 应被 cooldown 跳过");
        }
    }

    #[test]
    fn clear_cooldown_re_enables_credential() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(3600),
            CooldownReason::Auth,
        );
        assert!(r.next_for("openai/x", &[]).is_none());
        r.clear_cooldown("openai", "work");
        let nc = r.next_for("openai/x", &[]).unwrap();
        assert_eq!(nc.label, "work");
    }

    #[test]
    fn mark_cooldown_doubles_when_still_active() {
        // Phase 4 指数退避:第一次 60s cooldown 后**不 sleep** 立即第二次
        // 30s → 新 until ≈ now + max(60s × 2, 30s) = 120s,且
        // previous_until 留作审计。
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(60),
            CooldownReason::RateLimited {
                retry_after_ms: 60_000,
            },
        );
        // 立即第二次 mark(remaining ≈ 60s,requested = 30s)
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(30),
            CooldownReason::RateLimited {
                retry_after_ms: 30_000,
            },
        );
        // 验证:仍在 cooldown 且 healthy_clients 空(说明新 until > now)
        assert!(
            r.healthy_clients("openai/x").is_empty(),
            "backoff 后 credential 应仍在 cooldown"
        );
        // 验证 previous_until 字段:读 cooldowns map 内部
        let cd_map = r.cooldowns.read();
        let entry = cd_map.get("openai").unwrap().get("work").unwrap();
        assert!(
            entry.previous_until.is_some(),
            "second mark 应记录 previous_until"
        );
        // 新 until - now 应 ≈ 120s(允许浮点误差)
        let remaining = entry.until.saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_secs(115) && remaining <= Duration::from_secs(125),
            "expected ~120s backoff, got {remaining:?}"
        );
    }

    #[test]
    fn mark_cooldown_uses_max_when_requested_larger() {
        // remaining 短(30s)但 requested 大(3600s)→ 应走 requested,
        // 不走 2 × remaining = 60s。
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(30),
            CooldownReason::RateLimited {
                retry_after_ms: 30_000,
            },
        );
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(3600),
            CooldownReason::Auth,
        );
        let cd_map = r.cooldowns.read();
        let entry = cd_map.get("openai").unwrap().get("work").unwrap();
        let remaining = entry.until.saturating_duration_since(Instant::now());
        // remaining ≈ 3600s(Auth),不应被截到 60s
        assert!(
            remaining >= Duration::from_secs(3590),
            "expected ~3600s (requested wins over 2x remaining), got {remaining:?}"
        );
    }

    #[test]
    fn mark_cooldown_caps_at_one_hour() {
        // 旧 cooldown 接近 1h,新 requested 0s → 2 × remaining > 1h → cap。
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(3600),
            CooldownReason::Auth,
        );
        // 立即再 mark(remaining ≈ 3600s,requested = 0)
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(0),
            CooldownReason::RateLimited { retry_after_ms: 0 },
        );
        let cd_map = r.cooldowns.read();
        let entry = cd_map.get("openai").unwrap().get("work").unwrap();
        let remaining = entry.until.saturating_duration_since(Instant::now());
        // cap 1h(3600s),不应超过
        assert!(
            remaining <= Duration::from_secs(3600),
            "cap should hold, got {remaining:?}"
        );
    }

    #[test]
    fn mark_cooldown_first_time_has_no_previous() {
        // 首次进 cooldown:previous_until = None。
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(60),
            CooldownReason::Auth,
        );
        let cd_map = r.cooldowns.read();
        let entry = cd_map.get("openai").unwrap().get("work").unwrap();
        assert!(entry.previous_until.is_none());
    }

    #[test]
    fn unregister_removes_provider() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        assert_eq!(r.list(), vec!["openai".to_string()]);
        r.unregister("openai");
        assert!(r.list().is_empty());
        assert!(r.next_for("openai/x", &[]).is_none());
    }

    #[test]
    fn upsert_credential_replaces_existing_label() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.upsert_credential("openai", entry("work", 5)); // 改 weight
        // weight=5 单 entry → 6 次都应拿 work
        for _ in 0..6 {
            assert_eq!(r.next_for("openai/x", &[]).unwrap().label, "work");
        }
    }

    #[test]
    fn upsert_credential_appends_new_label() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1)]));
        r.upsert_credential("openai", entry("personal", 1));
        let labels: Vec<String> = (0..4)
            .map(|_| r.next_for("openai/x", &[]).unwrap().label)
            .collect();
        // work + personal 循环
        assert_eq!(labels, vec!["work", "personal", "work", "personal"]);
    }

    #[test]
    fn remove_credential_drops_entry() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1), entry("personal", 1)]));
        r.remove_credential("openai", "work");
        // 只剩 personal
        for _ in 0..3 {
            assert_eq!(r.next_for("openai/x", &[]).unwrap().label, "personal");
        }
    }

    #[test]
    fn healthy_clients_excludes_cooldown() {
        let r = ModelRegistry::new();
        r.register_pool("openai", pool(vec![entry("work", 1), entry("personal", 1)]));
        r.mark_cooldown(
            "openai",
            "work",
            Duration::from_secs(3600),
            CooldownReason::Auth,
        );
        let healthy = r.healthy_clients("openai/x");
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].label, "personal");
    }

    #[test]
    fn empty_pool_returns_none() {
        let r = ModelRegistry::new();
        // register_pool 不带 entries 也能跑(无 client → 全部不可用)
        r.register_pool("openai", CredentialPool::default());
        assert!(r.next_for("openai/x", &[]).is_none());
        assert!(r.healthy_clients("openai/x").is_empty());
    }

    #[test]
    fn unknown_provider_returns_none() {
        let r = ModelRegistry::new();
        assert!(r.next_for("missing/x", &[]).is_none());
        assert!(r.resolve("missing/x").is_none());
        assert!(r.get("missing").is_none());
    }

    /// v1.0.0-rc2: 3 个内置 client 各自 override `provider_kind()`。
    /// 通过 `registry.next_for` 拿到 `NextClient`,再调其
    /// `client.provider_kind()` 验 enum 正确。
    #[test]
    fn provider_kind_stub_defaults_to_custom() {
        use crate::ProviderKind;

        let r = ModelRegistry::new();
        r.register_pool(
            "anthropic",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: as_client(stub()),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );

        // 测试 stub 默认 ProviderKind::Custom(未 override);真实 builtin
        // client 在 providers/{anthropic,openai,ollama}.rs 内 override,
        // 见各自 `impl ModelClient for XxxClient` 块的 `provider_kind`。
        // 此处验证 registry 层 NextClient 透传无误。
        let nc = r.next_for("anthropic/x", &[]).unwrap();
        assert_eq!(nc.client.provider_kind(), ProviderKind::Custom);

        // 真实 builtin client 的 override 验证见 providers/*/tests 模块,
        // 不在本 registry 单元测范围。
    }
}
