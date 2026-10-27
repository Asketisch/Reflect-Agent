//! 配额持久化集成测试。独立二进制隔离进程级路径环境变量,
//! 避免并发单测里的其他 QuotaTracker 把此处的恢复样本覆写。

use std::time::Duration;
use zhuye_llm::{QuotaConfig, QuotaTracker};

static QUOTA_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct QuotaStateGuard {
    path: std::path::PathBuf,
    prior: Option<std::ffi::OsString>,
}

impl QuotaStateGuard {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("zhuye-quota-test-{}.json", uuid::Uuid::new_v4()));
        let prior = std::env::var_os("ZHUYE_QUOTA_STATE");
        unsafe {
            std::env::set_var("ZHUYE_QUOTA_STATE", &path);
        }
        Self { path, prior }
    }
}

impl Drop for QuotaStateGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prior {
                Some(value) => std::env::set_var("ZHUYE_QUOTA_STATE", value),
                None => std::env::remove_var("ZHUYE_QUOTA_STATE"),
            }
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("json.tmp"));
    }
}

fn cfg(window_secs: u64, max_tokens: u64) -> QuotaConfig {
    QuotaConfig {
        window_secs,
        max_tokens,
        check_via: None,
    }
}

/// save → 新 tracker load → 用量与耗尽判定保持(重启语义)。
/// 独立测试进程内串行使用唯一临时文件,避免其他 tracker 覆写同一份用量。
#[test]
fn save_then_load_restores_window() {
    let _serial = QUOTA_ENV_LOCK.lock().unwrap();
    let _state = QuotaStateGuard::new();
    let t1 = QuotaTracker::new();
    t1.register("anthropic", "plan-a", cfg(18000, 1000));
    t1.record_usage("anthropic", "plan-a", 700);
    assert!(!t1.is_exhausted("anthropic", "plan-a"), "700/1000 未耗尽");
    t1.save_state_if_configured();

    // 新进程等价:全新 tracker,load 后窗口恢复;继续累加至耗尽。
    let t2 = QuotaTracker::new();
    t2.register("anthropic", "plan-a", cfg(18000, 1000));
    t2.load_state();
    assert_eq!(
        t2.used_tokens("anthropic", "plan-a"),
        Some(700),
        "load 后用量应恢复"
    );
    t2.record_usage("anthropic", "plan-a", 300);
    assert!(
        t2.is_exhausted("anthropic", "plan-a"),
        "恢复的 700 + 新增 300 应触达上限"
    );
    assert!(t2.is_exhausted("anthropic", "plan-a"), "耗尽判定跨重启保持");
}

/// 已过期的窗口在 load 时按语义丢弃(不恢复陈旧用量)。
#[test]
fn load_state_drops_expired_windows() {
    let _serial = QUOTA_ENV_LOCK.lock().unwrap();
    let _state = QuotaStateGuard::new();
    let t1 = QuotaTracker::new();
    t1.register("anthropic", "short", cfg(1, 100));
    t1.record_usage("anthropic", "short", 90);
    t1.save_state_if_configured();
    // 等 1 秒窗口过期。
    std::thread::sleep(Duration::from_millis(1100));

    let t2 = QuotaTracker::new();
    t2.register("anthropic", "short", cfg(1, 100));
    t2.load_state();
    assert_eq!(
        t2.used_tokens("anthropic", "short"),
        None,
        "过期窗口不应恢复"
    );
}
