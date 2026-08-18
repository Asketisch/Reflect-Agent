//! 测试用 HOME 串行化 —— `reflect-cli` 内所有改 `$HOME` 的测试共用一把锁,
//! 避免 `plugin` / `task` 等模块各自持锁时 parallel runner 仍互相污染。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// 获取全局 HOME 锁并把 `$HOME` 指到 `home`。Guard  drop 前其它测试不能改 HOME。
pub fn lock_home(home: &Path) -> MutexGuard<'static, ()> {
    let guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_home(home);
    guard
}

/// 在已持有 `HOME_LOCK` 时重设 `$HOME`(例如 async 边界后 reclaim)。
pub fn set_home(home: &Path) {
    // SAFETY: 调用方必须已通过 `lock_home` 串行化。
    unsafe { std::env::set_var("HOME", home) };
}
