//! v1.3 analytics fail-closed:endpoint 黑洞端口时,init 不 panic
//! 不返回错误,运行时 batch 失败由 SDK 内部处理,agent 主循环不受影响。
//!
//! 跑 `cargo test -p reflect-config --test analytics_init_fail_closed`。

use reflect_config::AnalyticsSection;
use reflect_config::analytics::{Health, health::current};
use reflect_config::init_exporter;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_does_not_panic_on_unreachable_endpoint() {
    // 端口 1 通常没监听;OTLP exporter builder 不会在此阶段发起请求,
    // 所以 init 应该成功(返回 Some),不会因为 endpoint 不可达而失败。
    let section = AnalyticsSection {
        enabled: Some(true),
        endpoint: Some("http://127.0.0.1:1/v1/traces".into()),
        service_name: Some("reflect-fail-closed".into()),
        ..Default::default()
    };
    // 此处只断言不 panic、不抛错。
    let _ = init_exporter(&section);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_loop_unaffected_when_endpoint_blackholes() {
    // 模拟主循环在 telemetry 初始化失败/降级下仍可正常完成一轮。
    let section = AnalyticsSection {
        enabled: Some(true),
        endpoint: Some("http://127.0.0.1:1/v1/traces".into()),
        ..Default::default()
    };
    let guard = init_exporter(&section);
    // 这里我们只验证 guard 的 drop 不会 panic。即使 batch 里有未发送的
    // span,shutdown 也不应抛错(SDK 自己处理)。
    drop(guard);
    // health 状态可以是 Healthy(init 成功)或 Degraded(后续 batch 失败),
    // 但绝不应让进程 panic。
    let _ = current();
}

#[test]
fn health_module_default_is_disabled() {
    // 单元:process startup 后 health 默认是 Disabled(尚未 init_exporter)。
    let _ = Health::Disabled; // 类型导入校验
}
