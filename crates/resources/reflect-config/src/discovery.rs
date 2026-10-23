//! 模型清单发现 —— 把「运行时才知道」的 model 特殊值解析成真实模型 id。
//!
//! ## 背景
//!
//! 免费模型池的成员集合由**上游**决定,随时可能增删。因此配置里写死
//! 模型 id 必然腐烂。本模块让配置可以写**指针**而不是字面量:
//!
//! ```toml
//! [openai]
//! model = "free"      # 运行时查 provider 清单,按免费规则挑一个
//! ```
//!
//! 这与既有的 `REFLECT_MODEL=mock` 完全同构 —— **显式 opt-in 的特殊值**,
//! 由用户主动写下,不是「没配就编一个默认」。
//!
//! ## 失败语义
//!
//! **不静默回落,绝不编造 id**:
//! - provider 没有清单端点 → 报错,提示显式配置 model;
//! - 清单拉取失败 → 报错,带上原始原因;
//! - 清单里没有免费模型 → 报错,提示用 `reflect models list` 查看全部。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reflect_llm::{ModelInfo, ModelRegistry};
use tokio_util::sync::CancellationToken;

use crate::error::ConfigError;
use crate::schema::{ModelDiscoverySection, ReflectConfig};

/// `model` 特殊值:运行时从清单里挑一个免费模型。
pub const MODEL_SPEC_FREE: &str = "free";

/// 清单接口默认超时(秒)。
pub const DEFAULT_DISCOVERY_TIMEOUT_SECS: u64 = 20;

/// 清单缓存 TTL(秒)—— 进程内,避免每次热重载都打一次上游。
pub const DEFAULT_CATALOG_CACHE_TTL_SECS: u64 = 300;

/// 进程内清单缓存:provider → (取回时刻, 条目)。
///
/// 清单是 provider 级的事实,短时间内重复拉取没有新信息 —— 热重载、
/// CLI 连续调用、live 层的模型探测都走这份缓存。
///
/// 类型用别名拆开(`clippy::type_complexity` 对
/// `OnceLock<Mutex<HashMap<..>>>` 这种嵌套泛型直接报错)。
type CatalogCache = Mutex<HashMap<String, (Instant, Vec<ModelInfo>)>>;

static CATALOG_CACHE: OnceLock<CatalogCache> = OnceLock::new();

fn catalog_cache() -> &'static CatalogCache {
    CATALOG_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 一条 provider 的模型清单(按 provider 聚合)。
#[derive(Debug, Clone, Default)]
pub struct DiscoveredCatalog {
    /// provider 名。
    pub provider: String,
    /// 该 provider 报出的条目(多条凭据的结果按 id 去重)。
    pub models: Vec<ModelInfo>,
    /// 哪些凭据成功报出了清单。
    pub sources: Vec<String>,
}

/// 判定某个 model 值是否是「运行时解析」的特殊值。
///
/// 目前只有 `free` 一个 —— 与 `mock` 一样是显式 opt-in 的保留字。
pub fn is_dynamic_model(model: &str) -> bool {
    model.trim() == MODEL_SPEC_FREE
}

/// 拉取某 provider 的模型清单(合并池内所有凭据的结果)。
///
/// 池内多条凭据指向同一端点时结果会重复 —— 按 id 去重后返回。
/// 单条凭据失败**不**中断整体:能拿到几条算几条,全失败才报错。
///
/// 结果进进程内 TTL 缓存(见 [`DEFAULT_CATALOG_CACHE_TTL_SECS`]);缓存命中
/// 时不再打上游。
pub async fn fetch_catalog(
    registry: &ModelRegistry,
    provider: &str,
    timeout_secs: u64,
) -> Result<DiscoveredCatalog, ConfigError> {
    let ttl = Duration::from_secs(DEFAULT_CATALOG_CACHE_TTL_SECS);
    if let Some(cached) = read_cache(provider, ttl) {
        return Ok(cached);
    }
    let catalog = fetch_catalog_uncached(registry, provider, timeout_secs).await?;
    write_cache(provider, &catalog.models);
    Ok(catalog)
}

/// 读缓存(未命中或已过期返回 `None`)。
fn read_cache(provider: &str, ttl: Duration) -> Option<DiscoveredCatalog> {
    let cache = catalog_cache().lock().ok()?;
    let (at, models) = cache.get(provider)?;
    if at.elapsed() > ttl {
        return None;
    }
    Some(DiscoveredCatalog {
        provider: provider.to_string(),
        models: models.clone(),
        sources: vec!["cache".to_string()],
    })
}

/// 写缓存。
fn write_cache(provider: &str, models: &[ModelInfo]) {
    if let Ok(mut cache) = catalog_cache().lock() {
        cache.insert(provider.to_string(), (Instant::now(), models.to_vec()));
    }
}

/// 清空清单缓存。
///
/// provider 池重建(配置热重载 / 换 base_url / 换凭据)时必须调用 ——
/// 否则新端点的清单会被旧端点的条目顶掉(直到 TTL 到期)。
pub fn clear_catalog_cache() {
    if let Ok(mut cache) = catalog_cache().lock() {
        cache.clear();
    }
}

/// 绕过缓存拉取清单(测试与强制刷新用)。
pub async fn fetch_catalog_uncached(
    registry: &ModelRegistry,
    provider: &str,
    timeout_secs: u64,
) -> Result<DiscoveredCatalog, ConfigError> {
    let clients = registry.all_clients(provider);
    if clients.is_empty() {
        return Err(ConfigError::Build(format!(
            "provider '{provider}' 未注册任何凭据;无法获取模型清单"
        )));
    }

    let mut catalog = DiscoveredCatalog {
        provider: provider.to_string(),
        ..Default::default()
    };
    let mut last_error: Option<String> = None;
    let mut any_ok = false;

    for entry in clients {
        let token = CancellationToken::new();
        let timeout = Duration::from_secs(timeout_secs);
        // 用 `tokio::time::timeout` 包住清单调用:清单端点不应拖垮启动。
        match tokio::time::timeout(timeout, entry.client.list_models(token)).await {
            Ok(Ok(Some(models))) => {
                any_ok = true;
                catalog.sources.push(entry.label.clone());
                for model in models {
                    if !catalog.models.iter().any(|m| m.id == model.id) {
                        catalog.models.push(model);
                    }
                }
            }
            Ok(Ok(None)) => {
                // 该 provider 没有清单端点 —— 记下,不算失败(其他凭据可能有)。
                last_error = Some(format!(
                    "凭据 '{}' 的 provider 不提供模型清单端点",
                    entry.label
                ));
            }
            Ok(Err(e)) => {
                last_error = Some(format!("凭据 '{}': {e}", entry.label));
            }
            Err(_) => {
                last_error = Some(format!(
                    "凭据 '{}': 模型清单接口超时({timeout_secs}s)",
                    entry.label
                ));
            }
        }
    }

    if !any_ok {
        return Err(ConfigError::Build(format!(
            "无法获取 provider '{provider}' 的模型清单:{}",
            last_error.unwrap_or_else(|| "未知原因".to_string())
        )));
    }
    Ok(catalog)
}

/// 从清单里按免费规则挑一个模型(确定性:按 id 字典序)。
pub fn pick_free_model(
    catalog: &DiscoveredCatalog,
    section: &ModelDiscoverySection,
) -> Result<ModelInfo, ConfigError> {
    let rules = section.rules();
    rules
        .select(&catalog.models, section.prefer.as_deref())
        .cloned()
        .ok_or_else(|| {
            ConfigError::Build(format!(
                "provider '{}' 的模型清单里没有免费模型(共 {} 条);\
                 可用 `reflect models list` 查看全部,或在配置里显式写 model id",
                catalog.provider,
                catalog.models.len()
            ))
        })
}

/// 解析 `model = "free"` 这类运行时才知道的 model 值。
///
/// 返回 `Ok(None)` = 该值**不是**动态值(走既有的同步解析路径即可);
/// 返回 `Ok(Some(spec))` = 已解析成真实的 `"provider/model"` spec。
pub async fn resolve_dynamic_model(
    cfg: &ReflectConfig,
    registry: &ModelRegistry,
) -> Result<Option<String>, ConfigError> {
    let Some(provider) = cfg.active_provider() else {
        return Ok(None);
    };
    let Some(model) = cfg.resolve_model(provider) else {
        return Ok(None);
    };
    if !is_dynamic_model(&model) {
        return Ok(None);
    }

    let section = cfg.model_discovery.clone().unwrap_or_default();
    if !section.enabled {
        return Err(ConfigError::Build(
            "model 写成了 \"free\"(运行时从清单挑选),但 `[model_discovery] enabled = false`;\
             请打开该开关,或直接写具体的 model id"
                .to_string(),
        ));
    }

    let timeout_secs = section
        .timeout_secs
        .unwrap_or(DEFAULT_DISCOVERY_TIMEOUT_SECS);
    let catalog = fetch_catalog(registry, provider, timeout_secs).await?;
    let picked = pick_free_model(&catalog, &section)?;
    tracing::info!(
        provider = %provider,
        model = %picked.id,
        catalog_size = catalog.models.len(),
        "resolved dynamic model 'free' from provider catalog"
    );
    Ok(Some(format!("{provider}/{}", picked.id)))
}

/// 解析完整 spec:静态值走同步路径,动态值(`free`)走清单发现。
///
/// 这是给调用方(bootstrap / serve / 热重载)用的**统一入口**。
pub async fn resolve_model_spec(
    cfg: &ReflectConfig,
    registry: &ModelRegistry,
) -> Result<Option<String>, ConfigError> {
    match resolve_dynamic_model(cfg, registry).await? {
        Some(spec) => Ok(Some(spec)),
        None => Ok(cfg.resolved_model_spec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::provider::OpenAISection;

    #[test]
    fn 只有free是动态值() {
        assert!(is_dynamic_model("free"));
        assert!(is_dynamic_model(" free "));
        assert!(!is_dynamic_model("gpt-4o"));
        assert!(!is_dynamic_model("free-model"));
        assert!(!is_dynamic_model(""));
    }

    #[tokio::test]
    async fn 静态model不触发清单发现() {
        let cfg = ReflectConfig {
            active: crate::schema::ActiveSection {
                provider: Some("openai".into()),
                ..Default::default()
            },
            openai: Some(OpenAISection {
                api_key: Some("k".into()),
                model: Some("gpt-4o".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let registry = ModelRegistry::new();
        // provider 未注册 → 静态路径不应报错(不查清单)。
        let resolved = resolve_model_spec(&cfg, &registry)
            .await
            .expect("静态解析不应失败");
        assert_eq!(resolved.as_deref(), Some("openai/gpt-4o"));
    }
}
