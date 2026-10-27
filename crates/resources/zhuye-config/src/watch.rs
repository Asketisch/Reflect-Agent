//! 热重载:基于 `notify` 监听配置文件变更,debounce 后重新加载,
//! 通过 `tokio::sync::watch` 通知订阅者。
//!
//! ## 使用方式
//! ```ignore
//! let initial = ReflectConfig::load_default();   // 初始加载
//! let watcher = ConfigWatcher::spawn(path, initial)?;  // 启动监听
//! let mut rx = watcher.subscribe();              // 订阅变更
//! // 在另一个 task 里:
//! while rx.changed().await.is_ok() {             // 等待变更通知
//!     let new_cfg = rx.borrow().clone();         // 取新配置
//!     // ... 用 new_cfg 重建 provider client
//! }
//! ```
//!
//! ## 生命周期
//! `ConfigWatcher` 持有 `notify::RecommendedWatcher` + 后台 tokio task。
//! `Drop` 时 fs sender 关闭 → 桥接 thread 退出 → 异步 channel 关闭 → task 自然结束。
//!
//! ## Polling 兜底(v0.2.1)
//! macOS FSEvents 在快速连续 save 场景下偶发漏事件,250ms debounce 无法挽救。
//! 后台 `tokio::time::interval` 每 60s 主动 `load_from_file` 一次,与上次
//! 内容 hash 不同才推 watch channel —— 避免无变更时的无意义广播。
//! 生产用 `ConfigWatcher::spawn`(60s),测试用 `spawn_with_poll` 短间隔。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher, event::EventKind};
use tokio::sync::watch;

use crate::error::ConfigError;
use crate::schema::ReflectConfig;

/// debounce 窗口 —— 编辑器保存通常触发多次 fs 事件(先 truncate 再 write)。
const DEBOUNCE: Duration = Duration::from_millis(250);

/// polling 兜底周期 —— FSEvents 漏事件时仍能检测配置变更(v0.2.1 引入)。
const POLL_FALLBACK: Duration = Duration::from_secs(60);

/// 文件变更时通过 `watch` channel 派发新配置。
/// 解析失败时保持上一个有效值并 `tracing::warn`。
pub struct ConfigWatcher {
    rx: watch::Receiver<ReflectConfig>,
    /// 持有 watcher + 后台 task;Drop 后整个链路关闭。字段本身不直接读。
    #[allow(dead_code)]
    handle: WatcherHandle,
}

/// 持有底层 watcher + tokio task;`Drop` 后整个热重载链路关闭。
pub struct WatcherHandle {
    _watcher: RecommendedWatcher,
    _task: tokio::task::JoinHandle<()>,
}

impl ConfigWatcher {
    /// 启动一个 watcher,监听 `path` 所在目录(非递归),默认 60s polling 兜底。
    /// 初始值由调用者提供 (通常 `ReflectConfig::load_default()`)。
    pub fn spawn(path: PathBuf, initial: ReflectConfig) -> Result<Self, ConfigError> {
        Self::spawn_with_poll(path, initial, POLL_FALLBACK)
    }

    /// 同 `spawn`,但显式指定 polling 周期(测试用短间隔验证兜底路径)。
    pub fn spawn_with_poll(
        path: PathBuf,
        initial: ReflectConfig,
        poll_interval: Duration,
    ) -> Result<Self, ConfigError> {
        let (cfg_tx, cfg_rx) = watch::channel(initial);
        let (fs_tx, fs_rx) = mpsc::channel::<()>();

        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |_res| {
            // 不关心事件类型,只关心「文件变更过」。
            let _ = fs_tx.send(());
        })
        .map_err(|e| ConfigError::Watch(e.to_string()))?;

        let parent: PathBuf = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // 监听父目录(非递归) —— 这样 vim 等"rename+create"保存风格也能捕获。
        watcher
            .watch(&parent, RecursiveMode::NonRecursive)
            .map_err(|e| ConfigError::Watch(e.to_string()))?;

        let task = tokio::spawn(run_debounce_loop(fs_rx, path, cfg_tx, poll_interval));

        Ok(Self {
            rx: cfg_rx,
            handle: WatcherHandle {
                _watcher: watcher,
                _task: task,
            },
        })
    }

    /// 当前配置快照(便宜)。
    pub fn current(&self) -> ReflectConfig {
        self.rx.borrow().clone()
    }

    /// 拿一个新的订阅 receiver;每次调用独立。
    pub fn subscribe(&self) -> watch::Receiver<ReflectConfig> {
        self.rx.clone()
    }
}

async fn run_debounce_loop(
    fs_rx: mpsc::Receiver<()>,
    path: PathBuf,
    cfg_tx: watch::Sender<ReflectConfig>,
    poll_interval: Duration,
) {
    // std mpsc -> tokio mpsc 桥接,放在 blocking thread 避免阻塞 reactor。
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    std::thread::spawn(move || {
        // 收到事件就转发;fs_tx drop 后这里 recv() 返回 Err,thread 退出。
        while fs_rx.recv().is_ok() {
            if async_tx.send(()).is_err() {
                break;
            }
        }
    });

    // 初始 hash 用于 polling 兜底的去重(避免无变更时无意义广播)。
    let mut last_hash = hash_config(&cfg_tx.borrow());
    // 后台 polling 兜底 FSEvents 漏事件;`Skip` 行为避免 load 阻塞时堆积。
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 跳过第一次立即 tick,只在稳定后才开始比较。
    poll.tick().await;

    loop {
        tokio::select! {
            // 优先 fs 事件路径 —— `biased` 确保 polling 不抢 debounce 时机。
            biased;
            // fs 事件:debounce 后 reload;`None` 表示 fs channel 关闭 → 退出。
            fs_ev = async_rx.recv() => {
                if fs_ev.is_none() {
                    tracing::debug!("config watcher loop exited (fs channel closed)");
                    return;
                }
                tokio::time::sleep(DEBOUNCE).await;
                while async_rx.try_recv().is_ok() {}
                try_reload(&path, &cfg_tx, &mut last_hash, "fs").await;
            }
            // polling 兜底:周期 reload;hash 变了才广播。
            _ = poll.tick() => {
                try_reload(&path, &cfg_tx, &mut last_hash, "poll").await;
            }
        }
    }
}

/// 尝试 reload 配置并在内容变化时广播。`source` 仅用于 log 区分 fs/poll 路径。
async fn try_reload(
    path: &Path,
    cfg_tx: &watch::Sender<ReflectConfig>,
    last_hash: &mut u64,
    source: &str,
) {
    match crate::load::load_from_file(path) {
        Ok(new_cfg) => {
            let h = hash_config(&new_cfg);
            if h != *last_hash {
                *last_hash = h;
                tracing::info!(path = %path.display(), "config reloaded ({source})");
                // 发送失败 = 所有 receiver 都 drop 了,主 loop 会在下次 fs/poll 退出。
                let _ = cfg_tx.send(new_cfg);
            } else {
                tracing::debug!(path = %path.display(), "config reload no-op (unchanged)");
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config reload failed (keeping previous)"
            );
        }
    }
}

/// 对配置做内容 hash,用于 polling 兜底的去重。
/// 仅在同一进程内比较,因此 `DefaultHasher` 的非稳定 seed 无影响。
fn hash_config(cfg: &ReflectConfig) -> u64 {
    let mut h = DefaultHasher::new();
    // `ReflectConfig` 已派生 `Debug`,`{:?}` 输出包含所有字段,跨字段差异会反映在 hash 上。
    format!("{cfg:?}").hash(&mut h);
    h.finish()
}

/// 用于测试:只探测一个事件(无需真 watcher)。
#[cfg(test)]
async fn drain_once(
    fs_rx: mpsc::Receiver<()>,
    cfg_tx: watch::Sender<ReflectConfig>,
    path: PathBuf,
) {
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    std::thread::spawn(move || {
        while fs_rx.recv().is_ok() {
            if async_tx.send(()).is_err() {
                break;
            }
        }
    });
    if async_rx.recv().await.is_some() {
        tokio::time::sleep(DEBOUNCE).await;
        while async_rx.try_recv().is_ok() {}
        if let Ok(new_cfg) = crate::load::load_from_file(&path) {
            let _ = cfg_tx.send(new_cfg);
        }
    }
}

/// 事件过滤器(供未来想精修 hot-reload 触发条件时复用)。
#[allow(dead_code)]
fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[active]\nprovider = \"anthropic\"\n").unwrap();

        let initial = ReflectConfig::default();
        let watcher = ConfigWatcher::spawn(path.clone(), initial).unwrap();

        // 改文件。
        fs::write(
            &path,
            "[active]\nprovider = \"openai\"\n[openai]\napi_key = \"sk-new\"\n",
        )
        .unwrap();

        // 等待 debounce + 处理。
        tokio::time::sleep(Duration::from_millis(600)).await;

        let cfg = watcher.current();
        assert_eq!(cfg.active.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.openai.unwrap().api_key.as_deref(), Some("sk-new"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_reload_keeps_previous_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[active]\nprovider = \"anthropic\"\n").unwrap();

        let initial = ReflectConfig::default();
        let watcher = ConfigWatcher::spawn(path.clone(), initial).unwrap();

        // 写入无效 TOML。
        fs::write(&path, "this is = = invalid = toml @@").unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;

        // 默认初始值应当保留(因为 reload 失败)。
        let cfg = watcher.current();
        assert!(cfg.active.provider.is_none());
    }

    #[tokio::test]
    async fn drain_once_helper_loads_new_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[active]\nprovider = \"anthropic\"\n").unwrap();

        let (fs_tx, fs_rx) = mpsc::channel::<()>();
        let (cfg_tx, mut cfg_rx) = watch::channel(ReflectConfig::default());

        let path_for_task = path.clone();
        let task = tokio::spawn(drain_once(fs_rx, cfg_tx, path_for_task));

        // 写文件 + 触发事件。
        fs::write(
            &path,
            "[active]\nprovider = \"openai\"\n[openai]\napi_key = \"sk-x\"\n",
        )
        .unwrap();
        fs_tx.send(()).unwrap();
        drop(fs_tx);

        // 等待 drain_once 完成(它收到 fs_rx 关闭后退出)。
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let cfg = cfg_rx.borrow_and_update().clone();
        assert_eq!(cfg.active.provider.as_deref(), Some("openai"));
        drop(cfg_rx); // suppress unused warning
    }

    #[test]
    fn is_relevant_classifier() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(is_relevant(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_relevant(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_relevant(&EventKind::Access(
            notify::event::AccessKind::Read
        )));
    }

    /// Polling 兜底路径在 fs 事件被忽略时仍能检测到配置变更。
    ///
    /// 在测试中我们无法模拟「FSEvents 漏事件」(macOS specific),改用短 poll
    /// 间隔验证 polling 路径自身能 load + send;fs 路径可能也会 fire,但本
    /// 测试**只断言 cfg 最终一致**,两条路径任一成功即可。
    #[tokio::test(flavor = "current_thread")]
    async fn poll_fallback_catches_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[active]\nprovider = \"anthropic\"\n").unwrap();

        let initial = ReflectConfig::default();
        // 50ms poll 间隔 —— 走 spawn_with_poll 显式 override。
        let watcher =
            ConfigWatcher::spawn_with_poll(path.clone(), initial, Duration::from_millis(50))
                .unwrap();

        // 改文件(模拟 reload 触发场景)。
        fs::write(
            &path,
            "[active]\nprovider = \"openai\"\n[openai]\napi_key = \"sk-poll\"\n",
        )
        .unwrap();

        // 等 500ms > 250ms debounce + 余量,任一路径(fs 或 poll)触发 reload 即可。
        tokio::time::sleep(Duration::from_millis(500)).await;

        let cfg = watcher.current();
        assert_eq!(cfg.active.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.openai.unwrap().api_key.as_deref(), Some("sk-poll"));
    }

    /// Polling 路径检测到变更时不会重复 send 同一配置(hash dedup)。
    ///
    /// 多个 poll tick 内若无文件变更,`try_reload` 应通过 hash 短路,避免
    /// 重复污染 watch channel 订阅者。
    #[tokio::test(flavor = "current_thread")]
    async fn poll_path_dedupes_unchanged_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // 初始文件 = openai;但 channel initial = default —— 第一次 poll 必触发 send。
        fs::write(&path, "[active]\nprovider = \"openai\"\n").unwrap();

        let initial = ReflectConfig::default();
        // 关键:把 watcher 绑到变量,否则 `.subscribe()` 后临时 drop,后台 task 被取消。
        let watcher =
            ConfigWatcher::spawn_with_poll(path.clone(), initial, Duration::from_millis(50))
                .unwrap();
        let mut rx = watcher.subscribe();

        // `watch::Receiver::clone()` 出来的 receiver 初始 `has_changed=true`,
        // 第一次 `changed().await` 立即返回(不是真等 send)。先 mark seen。
        let _ = rx.borrow_and_update();

        // 等 3 个 poll tick(150ms),让第一次 polling 把 file → openai cfg 推上来。
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 此时应有 1 个 unread send(openai cfg);drain 掉。
        let _ = rx.changed().await;
        assert_eq!(rx.borrow().active.provider.as_deref(), Some("openai"));
        let _ = rx.borrow_and_update();

        // 再等几个 poll tick(文件未改),`changed()` 应一直等待(hash dedup 生效)。
        let no_change = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
        assert!(
            no_change.is_err(),
            "polling should NOT re-send unchanged config (hash dedup broken)"
        );
    }
}
