//! 分析遥测(OTLP/HTTP) — v1.3 起从 stub 升级为真功能。
//!
//! ## 设计
//!
//! 1. `init_exporter(section)` — 配置启用时启动 OTLP/HTTP exporter
//!    (`opentelemetry-otlp` + `tracing-opentelemetry` +
//!    `opentelemetry_sdk::trace::TracerProvider`)。
//! 2. 启动成功 → 设置全局 `TracerProvider`,业务侧用 `#[tracing::instrument]`
//!    / `tracing::info_span!` 自动转 OTLP span。
//! 3. 返回 `ExporterGuard`,`Drop` 时调用 `TracerProvider::shutdown()`
//!    强制 flush in-flight batch 再关闭 provider。
//!
//! ## 失败安全(fail-closed)
//!
//! `init_exporter` 任何阶段失败都返回 `None` + `tracing::warn!` 一次,
//! **不抛错、不阻塞** agent 主循环。`health` 模块记录最近状态供
//! `doctor` 显示。

use std::collections::HashMap;
use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::TracerProvider;
use tracing::warn;

use crate::schema::AnalyticsSection;

pub mod health;

pub use health::Health;

/// 启动 OTLP exporter。
///
/// - `enabled=false` 或 `enabled=None` → 返回 `None`,不修改全局 tracer。
/// - `endpoint` 为空 → 返回 `None`,记 degraded。
/// - exporter / provider 构建失败 → 返回 `None`,记 degraded + warn。
/// - 成功 → 设置全局 tracer provider,返回 `ExporterGuard`。
pub fn init_exporter(section: &AnalyticsSection) -> Option<ExporterGuard> {
    if !is_enabled(Some(section)) {
        tracing::trace!("analytics stub: disabled");
        health::set_disabled();
        return None;
    }

    let endpoint = section.endpoint.as_deref().unwrap_or("");
    let service = section.service_name.as_deref().unwrap_or("reflect-agent");
    let flush_ms = section.flush_timeout_ms.unwrap_or(2000);

    if endpoint.is_empty() {
        warn!("analytics enabled but endpoint is empty; skipping init");
        health::set_degraded("missing endpoint");
        return None;
    }

    let exporter = match SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_headers(headers_to_hashmap(&section.headers))
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "analytics: failed to build OTLP exporter");
            health::set_degraded(&format!("exporter build: {e}"));
            return None;
        }
    };

    let resource = Resource::new(vec![KeyValue::new("service.name", service.to_string())]);

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build();

    // 注册为全局,后续 `tracing_opentelemetry::layer().with_tracer(
    // opentelemetry::global::tracer("reflect"))` 会自动复用。
    opentelemetry::global::set_tracer_provider(provider.clone());

    tracing::info!(
        service = service,
        endpoint = endpoint,
        flush_timeout_ms = flush_ms,
        "analytics: OTLP exporter started"
    );

    health::set_healthy();
    Some(ExporterGuard {
        provider: Some(Arc::new(provider)),
    })
}

/// OTLP exporter guard。drop 时调用 `TracerProvider::shutdown()`
/// 强制 flush in-flight batch 再关闭 provider。
pub struct ExporterGuard {
    provider: Option<Arc<TracerProvider>>,
}

impl ExporterGuard {
    /// 取出一个具体类型的 `Tracer` 用于 `OpenTelemetryLayer::with_tracer`。
    ///
    /// 不能用 `opentelemetry::global::tracer(...)`(返回 `BoxedTracer`,
    /// 不实现 `PreSampledTracer`),必须从 `TracerProvider` 自身取。
    ///
    /// # Panics
    /// 若 guard 已被 `take`(Drop 之前显式移走 provider),该函数会 panic。
    /// 正常用法下 caller 在 guard 存活期间持有它,不会触发。
    pub fn tracer(&self, name: &'static str) -> opentelemetry_sdk::trace::Tracer {
        match self.provider.as_ref() {
            Some(provider) => provider.tracer(name),
            None => panic!("ExporterGuard::tracer called after provider taken"),
        }
    }
}

impl Drop for ExporterGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // TracerProvider::shutdown 内部走 BatchSpanProcessor::force_flush,
            // 不在调用线程上阻塞(provider 自己的 worker 上 flush)。
            if let Err(e) = provider.shutdown() {
                warn!(error = %e, "analytics: exporter shutdown returned error");
            } else {
                tracing::debug!("analytics: exporter flushed and shutdown");
            }
        }
    }
}

/// doctor 状态行:展示 enabled / service / endpoint / health 四段。
pub fn status_line(section: Option<&AnalyticsSection>) -> String {
    match section {
        Some(s) if is_enabled(Some(s)) => {
            let svc = s.service_name.as_deref().unwrap_or("reflect-agent");
            let ep = s.endpoint.as_deref().unwrap_or("(none)");
            format!(
                "analytics: enabled service={svc} endpoint={ep} health={}",
                health::current()
            )
        }
        _ => "analytics: stub disabled — 设置 [analytics] enabled=true".into(),
    }
}

/// 配置是否启用遥测。
pub fn is_enabled(section: Option<&AnalyticsSection>) -> bool {
    section.and_then(|s| s.enabled).unwrap_or(false)
}

/// 默认 `service.name` —— 用于未配置时回退。
pub fn default_service_name() -> &'static str {
    "reflect-agent"
}

fn headers_to_hashmap(h: &std::collections::BTreeMap<String, String>) -> HashMap<String, String> {
    h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `health::CURRENT` 是进程级全局单例:`cargo test` 并行跑多个用例时,
    /// 「写 → 读」窗口若被别的用例的写入跨越(如邻居的 `set_disabled`
    /// 覆盖 `Degraded`),断言会被污染。此处用锁串行化断言全局的用例
    /// 临界区。
    static HEALTH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 容忍锁中毒:前序用例 panic 过也照常拿到锁。
    fn lock_health() -> std::sync::MutexGuard<'static, ()> {
        HEALTH_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn status_disabled_by_default() {
        assert!(status_line(None).contains("disabled"));
    }

    #[test]
    fn status_disabled_when_enabled_false() {
        let s = AnalyticsSection {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(status_line(Some(&s)).contains("disabled"));
    }

    #[test]
    fn status_enabled_shows_service_and_endpoint() {
        let s = AnalyticsSection {
            enabled: Some(true),
            endpoint: Some("http://localhost:4318/v1/traces".into()),
            service_name: Some("test-svc".into()),
            ..Default::default()
        };
        let line = status_line(Some(&s));
        assert!(line.contains("test-svc"));
        assert!(line.contains("localhost:4318"));
        // init_exporter 没跑过,health 仍是默认 Disabled
        assert!(line.contains("health=disabled") || line.contains("health=degraded"));
    }

    #[test]
    fn is_enabled_respects_none() {
        assert!(!is_enabled(None));
    }

    #[test]
    fn is_enabled_respects_explicit_value() {
        let s = AnalyticsSection {
            enabled: Some(true),
            ..Default::default()
        };
        assert!(is_enabled(Some(&s)));
        let s = AnalyticsSection {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(!is_enabled(Some(&s)));
    }

    #[test]
    fn init_exporter_disabled_returns_none() {
        let _h = lock_health();
        let s = AnalyticsSection {
            enabled: Some(false),
            endpoint: Some("http://localhost:1/v1/traces".into()),
            ..Default::default()
        };
        let g = init_exporter(&s);
        assert!(g.is_none());
    }

    #[test]
    fn init_exporter_empty_endpoint_returns_none_and_marks_degraded() {
        let _h = lock_health();
        let s = AnalyticsSection {
            enabled: Some(true),
            endpoint: Some(String::new()),
            ..Default::default()
        };
        let g = init_exporter(&s);
        assert!(g.is_none());
        assert!(matches!(health::current(), Health::Degraded(_)));
    }
}
