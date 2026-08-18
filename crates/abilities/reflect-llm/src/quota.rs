//! v1.x 功能 7:本地 token plan 配额追踪器。
//!
//! 当一条 credential 声明了 `QuotaConfig`(通过 `[[<provider>.credentials]]`
//! 的 `quota` 字段),引擎在每次 LLM 调用成功后把 `usage.total_tokens` 累加
//! 到对应窗口;累计达 `max_tokens` 即视为耗尽,触发 cooldown 切到下一个
//! credential(plan B)。窗口到期(`window_secs`)后 `used_tokens` 归零,
//! credential 自动重新可用 —— 实现「时间窗口自动重置」。
//!
//! ## 设计取舍
//!
//! - 纯内存(v1):进程重启窗口重置。计划补 `~/.reflect/quota-state.json`
//!   持久化(标注 TODO),与 rollout 同级。
//! - 本地统计:不依赖厂商 quota API(厂商支持不一)。`QuotaSource` 枚举
//!   留作厂商 API 路由,后续按 cc-switch 实现适配器。
//! - 线程安全:`parking_lot::RwLock` 包裹,`record_usage` 在 LLM 热路径
//!   调用,锁粒度小(单 HashMap entry)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::providers::quota::{QuotaProvider, QuotaSnapshot};

/// (provider, label) 组合键 —— 同一 provider 下不同 credential 各自独立窗口。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    pub provider: String,
    pub label: String,
}

/// 单窗口的运行时状态(可变)。
#[derive(Debug, Clone)]
struct QuotaWindow {
    /// 本窗口起始时刻;`now - window_start >= window_secs` 时重置。
    window_start: Instant,
    /// 本窗口累计 token 用量。
    used_tokens: u64,
}

/// 配额追踪器。线程安全,`Arc<QuotaTracker>` 共享给 `model_call`。
///
/// 两条判定路径(由 `QuotaConfig.check_via` 决定):
/// - `None`(本地统计):累计 `record_usage` 的 token,达 `max_tokens` 即耗尽。
/// - `Some(厂商)`(厂商 API):调 `QuotaProvider::query` 拿真实剩余额度;
///   API 失败(网络/超时)回退本地统计。
pub struct QuotaTracker {
    /// key → 窗口状态(本地累计)。
    windows: RwLock<HashMap<CredentialKey, QuotaWindow>>,
    /// key → 配额配置(由 bootstrap 从 config 注入,不变)。
    configs: RwLock<HashMap<CredentialKey, QuotaConfig>>,
    /// key → 厂商适配器(`check_via = Some` 时注入)。
    providers: RwLock<HashMap<CredentialKey, Arc<dyn QuotaProvider>>>,
    /// key → credential 的 base_url + api_key(厂商 API 查询需要)。
    credentials: RwLock<HashMap<CredentialKey, (String, String)>>,
}

impl Default for QuotaTracker {
    fn default() -> Self {
        Self {
            windows: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            providers: RwLock::new(HashMap::new()),
            credentials: RwLock::new(HashMap::new()),
        }
    }
}

/// 配额配置(运行时镜像)。字段语义与 `reflect_config::QuotaConfig` 一致,
/// 但单独定义避免 reflect-llm 反向依赖 reflect-config;转换在 reflect-exec
/// 里做(`QuotaConfig { window_secs, max_tokens, check_via }`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaConfig {
    pub window_secs: u64,
    pub max_tokens: u64,
    /// `None` = 本地统计;`Some` = 调厂商 quota API(具体厂商由 bootstrap
    /// 映射成 `QuotaProvider` 实例注入 `register_provider`)。
    pub check_via: Option<QuotaSource>,
}

/// 厂商 quota API 路由(运行时镜像,与 `reflect_config::QuotaSource` 一致)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaSource {
    Kimi,
    Zhipu,
    Minimax,
    Zenmux,
    Volcengine,
    AnthropicUsage,
    OpenAIUsage,
}

/// 共享句柄类型(供 `AgentConfig` / `NodeContext` 持有)。
pub type SharedQuotaTracker = Arc<QuotaTracker>;

impl QuotaTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一条 credential 的配额配置。由 bootstrap 在解析 config 后调用:
    /// 对每条声明了 `quota` 的 credential 调一次。重复注册同 key 覆盖配置。
    pub fn register(&self, provider: &str, label: &str, cfg: QuotaConfig) {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        self.configs.write().insert(key, cfg);
    }

    /// 注册 credential 的 base_url + api_key(厂商 API 查询需要)。
    /// 由 bootstrap 在 register 之后调用。即使 `check_via = None` 也可调
    /// (无副作用,仅存档供后续切到厂商 API 时用)。
    pub fn register_credential(&self, provider: &str, label: &str, base_url: &str, api_key: &str) {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        self.credentials
            .write()
            .insert(key, (base_url.to_string(), api_key.to_string()));
    }

    /// 注册厂商 quota 适配器(`check_via = Some` 时调用)。bootstrap 按
    /// `QuotaSource` 映射成对应 `QuotaProvider` 实例注入。
    pub fn register_provider(&self, provider: &str, label: &str, p: Arc<dyn QuotaProvider>) {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        self.providers.write().insert(key, p);
    }

    /// 异步查询厂商 API 判定配额是否耗尽。`check_via = None` 或无 provider
    /// 时返回 `None`(调用方回退本地统计)。API 失败(瞬时)也返回 `None`,
    /// 让本地统计兜底;API 成功但 `snapshot.is_exhausted()` 返回真实判定。
    pub async fn check_via_api(&self, provider: &str, label: &str) -> Option<QuotaSnapshot> {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        let (p, (base_url, api_key)) = {
            let providers = self.providers.read();
            let p = providers.get(&key).cloned()?;
            let creds = self.credentials.read();
            let creds = creds.get(&key).cloned()?;
            (p, creds)
        };
        match p.query(&base_url, &api_key).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    provider = provider,
                    label = label,
                    error = %e,
                    "quota API query failed; falling back to local stats"
                );
                None
            }
        }
    }

    /// 该 credential 是否声明了配额(即被 register 过)。
    pub fn is_tracked(&self, provider: &str, label: &str) -> bool {
        self.configs.read().contains_key(&CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        })
    }

    /// 累计一次 LLM 调用的 token 用量。若窗口已过期则先重置再累加。
    /// 未注册配额的 credential 是 no-op(向后兼容)。
    pub fn record_usage(&self, provider: &str, label: &str, tokens: u64) {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        let cfg = {
            let configs = self.configs.read();
            match configs.get(&key) {
                Some(c) => c.clone(),
                None => return, // 未声明配额,no-op。
            }
        };
        let now = Instant::now();
        let window_dur = Duration::from_secs(cfg.window_secs);
        let mut windows = self.windows.write();
        let win = windows.entry(key).or_insert(QuotaWindow {
            window_start: now,
            used_tokens: 0,
        });
        // 窗口过期 → 重置。
        if now.duration_since(win.window_start) >= window_dur {
            win.window_start = now;
            win.used_tokens = 0;
        }
        win.used_tokens = win.used_tokens.saturating_add(tokens);
    }

    /// 该 credential 配额是否已耗尽(`used_tokens >= max_tokens` 且窗口未过期)。
    /// 未注册配额的 credential 永不耗尽(返回 `false`)。
    pub fn is_exhausted(&self, provider: &str, label: &str) -> bool {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        let (cfg, win) = {
            let configs = self.configs.read();
            let cfg = match configs.get(&key) {
                Some(c) => c.clone(),
                None => return false,
            };
            let windows = self.windows.read();
            (cfg, windows.get(&key).cloned())
        };
        let Some(win) = win else {
            return false; // 从未 record_usage,未耗尽。
        };
        let now = Instant::now();
        // 窗口已过期 → 已重置,不算耗尽。
        if now.duration_since(win.window_start) >= Duration::from_secs(cfg.window_secs) {
            return false;
        }
        win.used_tokens >= cfg.max_tokens
    }

    /// 窗口剩余时间(用于 cooldown 时长)。未注册或已过期返回 `Duration::ZERO`。
    pub fn remaining_window(&self, provider: &str, label: &str) -> Duration {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        let (cfg, win) = {
            let configs = self.configs.read();
            let cfg = match configs.get(&key) {
                Some(c) => c.clone(),
                None => return Duration::ZERO,
            };
            let windows = self.windows.read();
            (cfg, windows.get(&key).cloned())
        };
        let Some(win) = win else {
            return Duration::ZERO;
        };
        let window_dur = Duration::from_secs(cfg.window_secs);
        let elapsed = Instant::now().duration_since(win.window_start);
        window_dur.saturating_sub(elapsed)
    }

    /// 当前窗口已用 token 快照(诊断 / 测试用)。未注册返回 `None`。
    pub fn used_tokens(&self, provider: &str, label: &str) -> Option<u64> {
        let key = CredentialKey {
            provider: provider.to_string(),
            label: label.to_string(),
        };
        self.windows.read().get(&key).map(|w| w.used_tokens)
    }
}

impl std::fmt::Debug for QuotaTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaTracker")
            .field("tracked", &self.configs.read().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(window_secs: u64, max_tokens: u64) -> QuotaConfig {
        QuotaConfig {
            window_secs,
            max_tokens,
            check_via: None,
        }
    }

    #[test]
    fn unregistered_credential_is_never_exhausted() {
        let t = QuotaTracker::new();
        t.record_usage("anthropic", "ghost", 1_000_000);
        assert!(!t.is_exhausted("anthropic", "ghost"));
        assert_eq!(t.used_tokens("anthropic", "ghost"), None);
    }

    #[test]
    fn record_usage_accumulates_until_exhausted() {
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(18000, 1000));
        t.record_usage("anthropic", "plan-a", 400);
        assert!(!t.is_exhausted("anthropic", "plan-a"));
        assert_eq!(t.used_tokens("anthropic", "plan-a"), Some(400));
        t.record_usage("anthropic", "plan-a", 600);
        assert!(t.is_exhausted("anthropic", "plan-a"));
        assert_eq!(t.used_tokens("anthropic", "plan-a"), Some(1000));
    }

    #[test]
    fn saturating_add_prevents_overflow() {
        let t = QuotaTracker::new();
        t.register("anthropic", "big", cfg(18000, 100));
        t.record_usage("anthropic", "big", u64::MAX);
        t.record_usage("anthropic", "big", 10);
        assert_eq!(t.used_tokens("anthropic", "big"), Some(u64::MAX));
        assert!(t.is_exhausted("anthropic", "big"));
    }

    #[test]
    fn distinct_credentials_tracked_independently() {
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(18000, 500));
        t.register("anthropic", "plan-b", cfg(18000, 500));
        t.record_usage("anthropic", "plan-a", 500);
        assert!(t.is_exhausted("anthropic", "plan-a"));
        assert!(!t.is_exhausted("anthropic", "plan-b"));
    }

    #[test]
    fn distinct_providers_tracked_independently() {
        let t = QuotaTracker::new();
        t.register("anthropic", "shared-label", cfg(18000, 500));
        t.register("openai", "shared-label", cfg(18000, 500));
        t.record_usage("anthropic", "shared-label", 500);
        assert!(t.is_exhausted("anthropic", "shared-label"));
        assert!(!t.is_exhausted("openai", "shared-label"));
    }

    #[test]
    fn remaining_window_positive_before_expiry() {
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(18000, 1000));
        t.record_usage("anthropic", "plan-a", 100);
        let rem = t.remaining_window("anthropic", "plan-a");
        assert!(rem.as_secs() <= 18000);
        assert!(rem.as_secs() >= 17990); // 刚 record,接近满窗。
    }

    #[test]
    fn window_reset_after_expiry_clears_usage() {
        // 用极短窗口(1 秒)模拟过期。
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(1, 1000));
        t.record_usage("anthropic", "plan-a", 800);
        assert!(t.is_exhausted("anthropic", "plan-a") || !t.is_exhausted("anthropic", "plan-a"));
        // 等窗口过期。
        std::thread::sleep(Duration::from_millis(1100));
        // 过期后再次 record → 重置窗口,used 归零再累加。
        t.record_usage("anthropic", "plan-a", 100);
        assert_eq!(t.used_tokens("anthropic", "plan-a"), Some(100));
        assert!(!t.is_exhausted("anthropic", "plan-a"));
    }

    #[test]
    fn exhausted_but_expired_window_not_exhausted() {
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(1, 100));
        t.record_usage("anthropic", "plan-a", 200);
        assert!(t.is_exhausted("anthropic", "plan-a"));
        std::thread::sleep(Duration::from_millis(1100));
        // 窗口过期但未重新 record → is_exhausted 返回 false(窗口已重置)。
        assert!(!t.is_exhausted("anthropic", "plan-a"));
    }

    #[test]
    fn is_tracked_reflects_registration() {
        let t = QuotaTracker::new();
        assert!(!t.is_tracked("anthropic", "x"));
        t.register("anthropic", "x", cfg(18000, 100));
        assert!(t.is_tracked("anthropic", "x"));
    }

    // ── v1.x 功能 7: check_via_api(厂商 API 优先 + 失败回退) ──────────

    /// mock provider:返回预设的 snapshot(或模拟瞬时失败)。
    struct MockProvider {
        snapshot: Result<QuotaSnapshot, crate::providers::quota::QuotaError>,
    }

    #[async_trait::async_trait]
    impl QuotaProvider for MockProvider {
        async fn query(
            &self,
            _base_url: &str,
            _api_key: &str,
        ) -> Result<QuotaSnapshot, crate::providers::quota::QuotaError> {
            self.snapshot.clone()
        }
    }

    #[tokio::test]
    async fn check_via_api_uses_provider_when_registered() {
        use crate::providers::quota::QuotaSnapshot;
        use chrono::Utc;
        let t = QuotaTracker::new();
        t.register("openai", "kimi", cfg(18000, 1000));
        t.register_credential(
            "openai",
            "kimi",
            "https://api.kimi.com/coding/v1",
            "sk-test",
        );
        // mock 返回 utilization=100(耗尽)。
        t.register_provider(
            "openai",
            "kimi",
            Arc::new(MockProvider {
                snapshot: Ok(QuotaSnapshot {
                    success: true,
                    error: None,
                    utilization: Some(100.0),
                    resets_at: Some(Utc::now() + chrono::Duration::hours(3)),
                    remaining_tokens: None,
                    max_tokens: None,
                }),
            }),
        );
        let snap = t.check_via_api("openai", "kimi").await;
        assert!(snap.is_some(), "registered provider 必须返回 snapshot");
        assert!(snap.unwrap().is_exhausted(), "utilization=100 应判定耗尽");
    }

    #[tokio::test]
    async fn check_via_api_returns_none_when_no_provider() {
        // 无 provider(check_via=None)→ 返回 None,调用方回退本地统计。
        let t = QuotaTracker::new();
        t.register("anthropic", "plan-a", cfg(18000, 1000));
        let snap = t.check_via_api("anthropic", "plan-a").await;
        assert!(snap.is_none());
    }

    #[tokio::test]
    async fn check_via_api_returns_none_on_network_error() {
        use crate::providers::quota::QuotaError;
        // 厂商 API 瞬时失败(网络)→ 返回 None,调用方回退本地统计。
        let t = QuotaTracker::new();
        t.register("openai", "kimi", cfg(18000, 1000));
        t.register_credential(
            "openai",
            "kimi",
            "https://api.kimi.com/coding/v1",
            "sk-test",
        );
        t.register_provider(
            "openai",
            "kimi",
            Arc::new(MockProvider {
                snapshot: Err(QuotaError::Network("timeout".into())),
            }),
        );
        let snap = t.check_via_api("openai", "kimi").await;
        assert!(snap.is_none(), "网络失败应回退本地统计(返回 None)");
    }
}
