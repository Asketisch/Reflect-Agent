//! `FileReadStateTracker` —— 会话级「agent 是否已读过该文件」状态存储。
//!
//! 由 `ReadBeforeEditHook`(`builtins/read_before_edit.rs`)用作 PreToolUse
//! gate 的状态来源:在 session 内追踪 agent 已成功 `read` 过的文件,
//! 配合 [`PlanModeGate`](crate::builtins::plan_mode_gate::PlanModeGate)
//! 提供"防误覆盖"层 —— 非安全边界,因为 `bash` 仍可绕过。
//!
//! ## 放置位置
//!
//! 该模块放在 `reflect-hooks` 而非 `reflect-tools`:
//!
//! - `reflect-tools` **已**依赖 `reflect-hooks`(`Cargo.toml`),反向
//!   依赖会形成循环;
//! - hook 自己通过 `crate::file_read_state::*` 直接使用;
//! - `read` tool 通过 `reflect_hooks::file_read_state::*` 标记
//!   (`Arc<FileReadStateTracker>` 由 bootstrap 注入 `ToolContext`)。
//!
//! ## 状态生命周期
//!
//! 纯内存,生命周期等于 `AgentThread`。`reflect` 重启后需重新 read。
//! 未来 v1.1+ 可能持久化到 `~/.reflect/read_state.json`(见 plan R5)。
//!
//! ## 并发模型
//!
//! `parking_lot::RwLock<HashMap<PathBuf, ReadRecord>>`。PreToolUse 取
//! 读锁(快),PostToolUse 取写锁。无需 async,所有路径都是 sync stats。
//!
//! 参见 `docs/tools-and-hooks.md §4.7`(待补充)的设计 rationale,
//! 特别是 **bash 绕过路径** 的免责说明。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use parking_lot::RwLock;
use tracing::warn;

/// 单次 read 观测:agent 在 `read_at` 读了 `path`,读时文件 mtime 是
/// `mtime_at_read`。`mtime_at_read = None` 表示 tombstone —— 读时
/// 文件不存在。
#[derive(Debug, Clone, Copy)]
pub struct ReadRecord {
    pub read_at: SystemTime,
    /// `None` = 文件读时不存在;tombstone 阻止 agent 在没 read 父目录
    /// 的情况下"幻觉"创建。
    pub mtime_at_read: Option<SystemTime>,
}

/// 为什么 write/edit 被拒绝。`into_reason_string` 渲染成可操作的
/// LLM 提示语。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// 本 session 内从未 read 过该文件。
    NeverRead { path: PathBuf },
    /// 读过,但 mtime 已漂移超过容差。
    StaleSinceRead {
        path: PathBuf,
        drift_tolerance_ms: u64,
    },
    /// 读过,但读时文件不存在(且现在仍不存在)—— 提示 agent 先 `glob`
    /// 确认路径合法,或 `write` 创建新文件。
    ReadWhileMissing { path: PathBuf },
}

impl DenyReason {
    /// 渲染为人类/LLM 可读的拒绝原因。Agent 看到这条 reason 后应
    /// 自助修正:补 `read` / 重 `read` / `glob` 路径 / 直接 `write`。
    pub fn into_reason_string(self) -> String {
        match self {
            Self::NeverRead { path } => format!(
                "edit denied: file '{}' was never read in this session. \
                 call `read` first to see its current contents.",
                path.display()
            ),
            Self::StaleSinceRead {
                path,
                drift_tolerance_ms,
            } => format!(
                "edit denied: file '{}' was modified since the last read \
                 (drift tolerance = {drift_tolerance_ms}ms). re-read with `read` \
                 before editing.",
                path.display()
            ),
            Self::ReadWhileMissing { path } => format!(
                "edit denied: file '{}' was not found at last `read` attempt. \
                 use `glob` to discover its actual path, or `write` to create it fresh.",
                path.display()
            ),
        }
    }
}

/// Per-session "agent 已读过哪些文件?" tracker。
///
/// 所有访问通过 `parking_lot::RwLock<HashMap<PathBuf, ReadRecord>>`。
/// 读路径(PreToolUse 检查)取读锁;写路径(PostToolUse 标记)取写锁。
/// 全部同步,无需 async。
pub struct FileReadStateTracker {
    inner: RwLock<HashMap<PathBuf, ReadRecord>>,
}

impl FileReadStateTracker {
    /// 空 tracker。生产代码用 `SharedFileReadState::new()` 获取 Arc 包装。
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Component-wise 把 `path` 相对 `workspace` 规范化 —— 镜像
    /// `reflect_tools::sandbox::resolve_sandbox_path` 的语义,但
    /// 不做 sandbox 检查(hook 层只关心"键一致")。
    ///
    /// - workspace 必须存在(否则返回 `io::Error`);
    /// - 已存在的路径段 `canonicalize`;
    /// - 不存在的 leaf 保留原样(`write` 会创建它);
    /// - 跳过 `CurDir`(`.`)、`ParentDir`(`..`)、`Prefix`、`RootDir`
    ///   —— 避免 `./foo` 与 `foo` 产生不同键,也拒绝上层逃逸
    ///   (实际 sandbox 由工具层 `PathEscape` 拦截)。
    pub fn canonicalize(workspace: &Path, path: &Path) -> std::io::Result<PathBuf> {
        use std::path::Component;
        let canonical_workspace = workspace.canonicalize()?;
        let mut final_path = canonical_workspace.clone();
        for component in path.components() {
            match component {
                Component::CurDir
                | Component::ParentDir
                | Component::Prefix(_)
                | Component::RootDir => {
                    continue;
                }
                Component::Normal(c) => {
                    final_path.push(c);
                    if final_path.exists() {
                        if let Ok(c) = final_path.canonicalize() {
                            final_path = c;
                        }
                    }
                    // 否则保留原样 —— `write` 时会创建。
                }
            }
        }
        Ok(final_path)
    }

    /// 记录 agent 在 `read_at` 成功 `read` 了 `path`,读时 mtime 是
    /// `mtime_at_read`(`None` = 文件不存在,tombstone)。
    ///
    /// `write`/`edit` 成功后由 hook 的 PostToolUse 回调 `forget`,
    /// 防止陈旧 read 记录误导下次校验。
    pub fn mark_read(&self, path: PathBuf, read_at: SystemTime, mtime_at_read: Option<SystemTime>) {
        self.inner.write().insert(
            path,
            ReadRecord {
                read_at,
                mtime_at_read,
            },
        );
    }

    /// 移除单条记录。返回是否原本存在。
    pub fn forget(&self, path: &Path) -> bool {
        self.inner.write().remove(path).is_some()
    }

    /// 清空全部记录。`/reset` 或 session 重启时调用。
    pub fn clear(&self) {
        self.inner.write().clear();
    }

    /// 判定当前 `path` 的 write/edit 是否安全。`drift_tolerance_ms`
    /// 是 mtime 容差(默认 500ms,可在 hook config 调)。
    ///
    /// **`drift_tolerance_ms == 0` 视为禁用漂移校验**——此时只验证
    /// 文件存在过(`Some(mtime_at_read)`),不比较 mtime 差值。运维
    /// 在严格监控文件系统事件或调试 hook 行为时,把容差置 0 等于
    /// 「关闭 stale 检测」,而非「任何 mtime 变化都视为 stale」。详见
    /// `reflect_config::schema::ReadBeforeEditSection` 注释。
    pub fn check_write_safe(&self, path: &Path, drift_tolerance_ms: u64) -> Result<(), DenyReason> {
        let record = {
            let map = self.inner.read();
            map.get(path).copied()
        };
        let Some(record) = record else {
            // First-write 例外:从未 read 过,且磁盘上也不存在 → 允许。
            // 这是「新建文件」的合法路径,工具层 sandbox 仍会拦截越界。
            // 文件**已存在**但未 read → 仍然 Deny(`NeverRead`),防止
            // agent 盲目覆盖。
            if !path.exists() {
                return Ok(());
            }
            return Err(DenyReason::NeverRead {
                path: path.to_path_buf(),
            });
        };
        match record.mtime_at_read {
            // Tombstone:read 时文件不存在。
            None => {
                if path.exists() {
                    // 文件后来出现(可能是其他工具创建),first-write 合法。
                    Ok(())
                } else {
                    // 文件仍然不存在 —— agent 幻觉路径,拒绝。
                    Err(DenyReason::ReadWhileMissing {
                        path: path.to_path_buf(),
                    })
                }
            }
            Some(read_mtime) => {
                let current_mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(_) => {
                        // 文件在 read 后消失了 —— 保守:视为漂移(避免
                        // agent 在错的状态下覆盖)。
                        return Err(DenyReason::StaleSinceRead {
                            path: path.to_path_buf(),
                            drift_tolerance_ms,
                        });
                    }
                };
                // 时钟回拨 / copy-on-write 反向 mtime —— 保守拒绝。
                // 该分支通常由 NTP 跳变 / 容器时钟漂移导致,运维需注意;
                // 此处 `warn!` 输出供 `tracing-subscriber` 收集。
                if current_mtime < read_mtime {
                    warn!(
                        path = %path.display(),
                        read_mtime = ?read_mtime,
                        current_mtime = ?current_mtime,
                        "read_before_edit: clock skew detected (current_mtime < read_mtime), \
                         treating as stale to avoid blind overwrites"
                    );
                    return Err(DenyReason::StaleSinceRead {
                        path: path.to_path_buf(),
                        drift_tolerance_ms,
                    });
                }
                let delta = current_mtime
                    .duration_since(read_mtime)
                    .unwrap_or(Duration::ZERO);
                // `drift_tolerance_ms == 0` 表示禁用 stale 检测 —— 跳过
                // mtime 比对,允许任意 delta。这是 schema 文档约定的语义。
                // 注意:仅绕过「mtime 漂移」比较,**不**绕过「文件消失」
                // 「时钟回拨」两条防御分支(已在更早处处理)。
                if drift_tolerance_ms == 0 {
                    return Ok(());
                }
                if delta > Duration::from_millis(drift_tolerance_ms) {
                    Err(DenyReason::StaleSinceRead {
                        path: path.to_path_buf(),
                        drift_tolerance_ms,
                    })
                } else {
                    Ok(())
                }
            }
        }
    }

    /// 当前追踪的路径数。测试 helper。
    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// 当前追踪的路径数是否为 0。测试 helper。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

impl Default for FileReadStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// `Arc<FileReadStateTracker>` 的便利别名。Hook 与 `read` tool 都持
/// 有同一 `Arc`,通过 bootstrap 构造。
pub type SharedFileReadState = std::sync::Arc<FileReadStateTracker>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// 互不干扰的临时 workspace。每个测试用唯一目录,避免跨 case 状态泄露。
    fn tmp_workspace() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "reflect_filestate_test_{}_{}",
            std::process::id(),
            n
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 1. `mark_read` 后立即 `check_write_safe` → Ok。
    #[test]
    fn mark_then_check_is_ok() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), SystemTime::now(), Some(mtime));
        assert!(tracker.check_write_safe(&canonical, 500).is_ok());
    }

    /// 2. 未 mark 的路径 → `NeverRead`。
    #[test]
    fn check_on_never_read_is_never_read() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        match tracker.check_write_safe(&canonical, 500) {
            Err(DenyReason::NeverRead { path }) => assert_eq!(path, canonical),
            other => panic!("expected NeverRead, got {other:?}"),
        }
    }

    /// 3. mtime 漂移超过容差 → `StaleSinceRead`。
    #[test]
    fn check_after_drift_is_stale() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let read_mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), read_mtime, Some(read_mtime));
        // 用 filetime 把 mtime 推到 +2s,远超 500ms 容差。
        let future = read_mtime + Duration::from_secs(2);
        filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();
        match tracker.check_write_safe(&canonical, 500) {
            Err(DenyReason::StaleSinceRead {
                drift_tolerance_ms, ..
            }) => {
                assert_eq!(drift_tolerance_ms, 500)
            }
            other => panic!("expected StaleSinceRead, got {other:?}"),
        }
    }

    /// 4. mtime 漂移 < 容差 → Ok。
    #[test]
    fn check_within_tolerance_is_ok() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let read_mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), read_mtime, Some(read_mtime));
        // +100ms,远小于 500ms 容差。
        let future = read_mtime + Duration::from_millis(100);
        filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();
        assert!(tracker.check_write_safe(&canonical, 500).is_ok());
    }

    /// 5a. Tombstone + 文件仍不存在 → `ReadWhileMissing`。
    #[test]
    fn tombstone_blocks_write_when_still_missing() {
        let ws = tmp_workspace();
        let tracker = FileReadStateTracker::new();
        let canonical = ws.join("ghost.txt");
        tracker.mark_read(canonical.clone(), SystemTime::now(), None);
        match tracker.check_write_safe(&canonical, 500) {
            Err(DenyReason::ReadWhileMissing { .. }) => {}
            other => panic!("expected ReadWhileMissing, got {other:?}"),
        }
    }

    /// 5b. Tombstone + 文件后来出现 → Ok(合法首次写入)。
    #[test]
    fn tombstone_allows_when_file_appears() {
        let ws = tmp_workspace();
        let tracker = FileReadStateTracker::new();
        let canonical = ws.join("ghost.txt");
        tracker.mark_read(canonical.clone(), SystemTime::now(), None);
        std::fs::write(&canonical, "hi").unwrap();
        assert!(tracker.check_write_safe(&canonical, 500).is_ok());
    }

    /// 6. `clear` 清空 map。
    #[test]
    fn clear_empties_map() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        tracker.mark_read(
            canonical.clone(),
            SystemTime::now(),
            Some(SystemTime::now()),
        );
        assert_eq!(tracker.len(), 1);
        tracker.clear();
        assert!(tracker.is_empty());
        assert!(matches!(
            tracker.check_write_safe(&canonical, 500),
            Err(DenyReason::NeverRead { .. })
        ));
    }

    /// 7. `./foo` 和 `foo` 规范化后键一致。
    #[test]
    fn canonicalize_relative_dots_match() {
        let ws = tmp_workspace();
        let file = ws.join("foo.txt");
        std::fs::write(&file, "x").unwrap();
        let p1 = FileReadStateTracker::canonicalize(&ws, Path::new("./foo.txt")).unwrap();
        let p2 = FileReadStateTracker::canonicalize(&ws, Path::new("foo.txt")).unwrap();
        assert_eq!(p1, p2);
        // 用文件 *实际* mtime —— 否则 now 略晚于 mtime 触发"时钟回拨"
        // 分支(conservative stale)。
        let mtime = std::fs::metadata(&p1).unwrap().modified().unwrap();
        let tracker = FileReadStateTracker::new();
        tracker.mark_read(p1.clone(), mtime, Some(mtime));
        assert!(tracker.check_write_safe(&p2, 500).is_ok());
    }

    /// 8. 软链与目标路径规范化后键一致(macOS / Linux)。
    #[cfg(unix)]
    #[test]
    fn canonicalize_follows_symlinks() {
        let ws = tmp_workspace();
        let target = ws.join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = ws.join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let p1 = FileReadStateTracker::canonicalize(&ws, Path::new("real.txt")).unwrap();
        let p2 = FileReadStateTracker::canonicalize(&ws, Path::new("alias.txt")).unwrap();
        assert_eq!(p1, p2);
    }

    /// 9. `forget` 移除单条记录。
    #[test]
    fn forget_removes_entry() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        tracker.mark_read(
            canonical.clone(),
            SystemTime::now(),
            Some(SystemTime::now()),
        );
        assert!(tracker.forget(&canonical));
        assert!(!tracker.forget(&canonical)); // 二次调用幂等
        assert!(matches!(
            tracker.check_write_safe(&canonical, 500),
            Err(DenyReason::NeverRead { .. })
        ));
    }

    /// 10. 时钟回拨(read_mtime 未来)→ `StaleSinceRead`(保守)。
    #[test]
    fn clock_skew_treated_as_stale() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let now = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        let future = now + Duration::from_secs(10);
        tracker.mark_read(canonical.clone(), future, Some(future));
        match tracker.check_write_safe(&canonical, 500) {
            Err(DenyReason::StaleSinceRead { .. }) => {}
            other => panic!("expected StaleSinceRead (clock skew), got {other:?}"),
        }
    }

    /// 10b. `drift_tolerance_ms = 0` 禁用漂移校验 —— 即便 mtime 推到
    /// 「未来 N 秒」也允许(只要文件存在过)。这是 schema 文档约定的
    /// 语义,运维临时关闭 stale 检测的开关。
    #[test]
    fn drift_zero_disables_stale_check() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let read_mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), read_mtime, Some(read_mtime));
        // 把 mtime 推到 +60s,远超任何合理容差。
        let future = read_mtime + Duration::from_secs(60);
        filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();
        assert!(
            tracker.check_write_safe(&canonical, 0).is_ok(),
            "drift=0 应禁用 stale 检测,允许任意 mtime 变化"
        );
        // 对照:容差 1ms 时同样场景会被拒。
        match tracker.check_write_safe(&canonical, 1) {
            Err(DenyReason::StaleSinceRead { .. }) => {}
            other => panic!("expected StaleSinceRead with drift=1, got {other:?}"),
        }
    }

    /// 10c. `drift_tolerance_ms = 0` 仍正确处理「文件读后消失」分支
    /// —— 这是保守的「文件状态异常」信号,与漂移无关。
    #[test]
    fn drift_zero_still_treats_missing_file_as_stale() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), mtime, Some(mtime));
        std::fs::remove_file(&canonical).unwrap();
        match tracker.check_write_safe(&canonical, 0) {
            Err(DenyReason::StaleSinceRead {
                drift_tolerance_ms, ..
            }) => {
                assert_eq!(drift_tolerance_ms, 0, "drift 参数原样透传给 reason");
            }
            other => panic!("expected StaleSinceRead even with drift=0, got {other:?}"),
        }
    }

    /// 11. read 后文件被外部删除 → `StaleSinceRead`(保守,防止覆盖)。
    #[test]
    fn file_disappeared_after_read() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let tracker = FileReadStateTracker::new();
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        tracker.mark_read(canonical.clone(), mtime, Some(mtime));
        std::fs::remove_file(&canonical).unwrap();
        match tracker.check_write_safe(&canonical, 500) {
            Err(DenyReason::StaleSinceRead { .. }) => {}
            other => panic!("expected StaleSinceRead (missing), got {other:?}"),
        }
    }

    /// 12. `DenyReason::into_reason_string` 渲染可操作的 LLM 提示。
    #[test]
    fn deny_reason_messages_are_actionable() {
        let r1 = DenyReason::NeverRead {
            path: PathBuf::from("/tmp/x.txt"),
        };
        let s1 = r1.into_reason_string();
        assert!(s1.contains("never read"));
        assert!(s1.contains("/tmp/x.txt"));
        assert!(s1.contains("read"));

        let r2 = DenyReason::StaleSinceRead {
            path: PathBuf::from("/tmp/y.txt"),
            drift_tolerance_ms: 500,
        };
        let s2 = r2.into_reason_string();
        assert!(s2.contains("modified"));
        assert!(s2.contains("500ms"));

        let r3 = DenyReason::ReadWhileMissing {
            path: PathBuf::from("/tmp/z.txt"),
        };
        let s3 = r3.into_reason_string();
        assert!(s3.contains("not found"));
        assert!(s3.contains("glob"));
    }

    /// 13. `SharedFileReadState` 通过 Arc 跨线程共享。
    #[test]
    fn shared_tracker_works_across_threads() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let shared: SharedFileReadState = std::sync::Arc::new(FileReadStateTracker::new());
        let canonical = FileReadStateTracker::canonicalize(&ws, Path::new("a.txt")).unwrap();
        // 用文件 *实际* mtime,不是 SystemTime::now() —— 否则 now > mtime
        // 触发"时钟回拨"分支(conservative stale)。
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        shared.mark_read(canonical.clone(), mtime, Some(mtime));
        let shared2 = shared.clone();
        let h = std::thread::spawn(move || shared2.check_write_safe(&canonical, 500).is_ok());
        assert!(h.join().unwrap());
    }

    /// 14. workspace 不存在 → `io::Error`。
    #[test]
    fn canonicalize_missing_workspace_errors() {
        let result = FileReadStateTracker::canonicalize(
            Path::new("/definitely/does/not/exist/anywhere"),
            Path::new("a.txt"),
        );
        assert!(result.is_err());
    }

    /// 15. `canonicalize` 对不存在的 leaf 保留原样(workspace+leaf),
    ///     不抛错;`write` 时会创建。这是 first-write 路径依赖的契约。
    #[test]
    fn canonicalize_keeps_nonexistent_leaf_as_is() {
        let ws = tmp_workspace();
        // 不创建 ghost.txt —— leaf 不存在。
        let p1 = FileReadStateTracker::canonicalize(&ws, Path::new("ghost.txt")).unwrap();
        let ws_canonical = ws.canonicalize().unwrap();
        assert_eq!(p1, ws_canonical.join("ghost.txt"));
        // 多级 missing leaf 同样保留
        let p2 = FileReadStateTracker::canonicalize(&ws, Path::new("a/b/c/new.txt")).unwrap();
        assert_eq!(p2, ws_canonical.join("a/b/c/new.txt"));
    }
}
