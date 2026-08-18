//! 端到端:ConfigWatcher 检测到 TOML 变更 → 重建 registry → 下一轮 `apply_to_registry` 拿到新 client。
//!
//! 这里只验证 watcher 派发新 cfg + `apply_to_registry` 不报错,而不真发 HTTP 请求。

use std::time::Duration;

use reflect_config::{ConfigWatcher, load_from_file};
use reflect_llm::ModelRegistry;

#[tokio::test(flavor = "current_thread")]
async fn watcher_triggers_registry_replacement_on_toml_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[anthropic]\napi_key = \"sk-a-1\"\n").unwrap();

    let initial = load_from_file(&path).unwrap();
    let watcher = ConfigWatcher::spawn(path.clone(), initial.clone()).unwrap();
    let registry = ModelRegistry::new();
    initial.apply_to_registry(&registry).unwrap();

    let mut rx = watcher.subscribe();

    // 改文件 —— 触发 notify event。
    std::fs::write(&path, "[anthropic]\napi_key = \"sk-a-2\"\n").unwrap();

    // 等待 watcher 检测 + debounce + reload + send。
    let new_cfg = tokio::time::timeout(Duration::from_secs(3), async {
        rx.changed().await.ok();
        rx.borrow().clone()
    })
    .await
    .expect("reload timed out");

    new_cfg.apply_to_registry(&registry).unwrap();
    assert_eq!(
        new_cfg.anthropic.as_ref().unwrap().api_key.as_deref(),
        Some("sk-a-2")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_toml_does_not_clear_old_clients() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[anthropic]\napi_key = \"sk-a-1\"\n").unwrap();

    let initial = load_from_file(&path).unwrap();
    let watcher = ConfigWatcher::spawn(path.clone(), initial.clone()).unwrap();
    assert!(watcher.current().anthropic.is_some());

    // 写非法 TOML —— watcher 会 warn 但保留上一个值。
    std::fs::write(&path, "garbage = = =").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let last = watcher.current();
    assert!(
        last.anthropic.is_some(),
        "previous valid config must be retained"
    );
    assert_eq!(last.anthropic.unwrap().api_key.as_deref(), Some("sk-a-1"));
}
