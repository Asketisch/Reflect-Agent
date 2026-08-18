//! v1.3 analytics e2e:用 wiremock 起 mock OTLP/HTTP 端点,
//! `init_exporter` 启动后 guard 的 tracer 创建的 span 应被 SDK 异步
//! POST 到 collector(我们用 shutdown 触发 flush)。
//!
//! 跑 `cargo test -p reflect-config --test analytics_e2e`。

use opentelemetry::trace::{Span, Tracer as _};
use reflect_config::analytics::ExporterGuard;
use reflect_config::{AnalyticsSection, init_exporter};
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_section(endpoint: String) -> AnalyticsSection {
    let mut headers = BTreeMap::new();
    headers.insert("x-api-key".into(), "test-token".into());
    AnalyticsSection {
        enabled: Some(true),
        endpoint: Some(endpoint),
        service_name: Some("reflect-test".into()),
        headers,
        flush_timeout_ms: Some(500),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_posts_spans_to_mock_otlp() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1..)
        .mount(&server)
        .await;

    let section = make_section(format!("{}/v1/traces", server.uri()));
    let guard: Option<ExporterGuard> = init_exporter(&section);
    assert!(guard.is_some(), "exporter should init successfully");

    // 用 guard 的 tracer 创建 span → 应被导出到 mock server。
    let tracer = guard.as_ref().unwrap().tracer("reflect-test");
    let mut span = tracer.start("agent.test.span");
    span.set_attribute(opentelemetry::KeyValue::new("test.key", "v"));
    span.end();

    // 主动 drop guard(触发 provider.shutdown → BatchSpanProcessor::force_flush)。
    drop(guard);

    // 给 mock server 一点时间收到请求。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 断言至少 1 个 POST 被命中。
    let received = server.received_requests().await.unwrap_or_default();
    assert!(!received.is_empty(), "expected at least 1 OTLP POST, got 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_disabled_returns_none() {
    let section = AnalyticsSection {
        enabled: Some(false),
        endpoint: Some("http://does-not-matter/v1/traces".into()),
        ..Default::default()
    };
    assert!(init_exporter(&section).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_empty_endpoint_returns_none_and_marks_degraded() {
    let section = AnalyticsSection {
        enabled: Some(true),
        endpoint: Some(String::new()),
        ..Default::default()
    };
    assert!(init_exporter(&section).is_none());
    let h = reflect_config::analytics::health::current();
    assert!(
        matches!(h, reflect_config::analytics::Health::Degraded(_)),
        "expected Degraded, got {h:?}"
    );
}
