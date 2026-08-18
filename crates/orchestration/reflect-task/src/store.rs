//! `TaskStore` trait + `InMemoryTaskStore` + `FileTaskStore`。
//!
//! 持久化后端抽象,与 `reflect-memory::MemoryStore` 同构(一个 trait +
//! 一个 in-memory + 一个 file-backed 实现)。Phase 0 落地的最小可用版本;
//! 跨进程文件锁用 per-list `tokio::sync::Mutex`(进程内序列化),跨进程
//! 互斥留给后续 phase 加 `fs2` advisory lock。
//!
//! 路径布局:
//! ```text
//! $REFLECT_HOME/tasks/<list>/    # list 目录
//!   .highwatermark   # 整数文本,记录该 list 历史最大 task id
//!   1.json           # 各任务文件(id 命名)
//!   2.json
//!   ...
//! ```
//!
//! ## 写入语义
//!
//! - 写文件用 `tmp + rename` 原子替换,避免半写状态。
//! - `next_id` 读 `.highwatermark` +1 写回;`save` + `next_id` 在同一把
//!   list-level Mutex 下串行执行,避免两个并发调用读到同一个高水位。
//! - `list` 读 list 目录下所有 `*.json`,过滤 `.highwatermark` 与 `.lock`,
//!   按 id 升序返回(失败的文件跳过 + `tracing::warn`)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tracing::warn;

use crate::error::TaskError;
use crate::model::{ListId, Task, TaskId, TaskStatus};

/// 任务存储后端的抽象接口。`Send + Sync` —— 多线程共享。
///
/// `next_id` 必须在每次创建前调用,确保 id 单调递增。`save` 是 upsert
/// 语义(覆盖已存在的 id);`delete` 真正删文件(区别于把 status 改为
/// `Deleted` 的软删除);`list` 默认按 id 升序返回,自动排除软删任务。
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// `list` 目录的绝对路径(用于 `output_path` 派生)。
    fn dir_for(&self, list: &ListId) -> PathBuf;

    /// 保存任务(`tmp + rename` 原子写)。
    async fn save(&self, list: &ListId, task: &Task) -> Result<(), TaskError>;

    /// 读取单个任务;不存在返回 `TaskError::NotFound`。
    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError>;

    /// 物理删除任务文件。
    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError>;

    /// 列出 list 下所有任务,按 id 升序。已软删(`status == Deleted`)的
    /// 默认排除 —— 由 `TaskManager` 在 `include_deleted=true` 时再包一层过滤。
    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError>;

    /// 分配下一个 task id(原子 +1)。每次调用都 +1 并持久化高水位。
    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError>;
}

// ── InMemoryTaskStore(内存任务存储)──────────────────────────

/// 内存后端。热路径(同 session 短期)使用,跨进程不持久化。
///
/// 内部用 `parking_lot::RwLock<HashMap<(ListId, TaskId), Task>>` 容纳数据;
/// `next_id` 用第二个 `RwLock<HashMap<ListId, TaskId>>` 维护高水位。
/// 操作在 `async fn` 内取 sync lock,锁粒度很小(几纳秒),不影响并发。
pub struct InMemoryTaskStore {
    map: RwLock<HashMap<(ListId, TaskId), Task>>,
    hwm: RwLock<HashMap<ListId, TaskId>>,
}

impl std::fmt::Debug for InMemoryTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.map.read();
        let hwm = self.hwm.read();
        f.debug_struct("InMemoryTaskStore")
            .field("task_count", &map.len())
            .field("list_count", &hwm.len())
            .finish()
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTaskStore {
    /// 新建空 store。
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            hwm: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    fn dir_for(&self, list: &ListId) -> PathBuf {
        PathBuf::from(format!("<in-memory>/{list}"))
    }

    async fn save(&self, _list: &ListId, task: &Task) -> Result<(), TaskError> {
        self.map
            .write()
            .insert((task.list_id.clone(), task.id), task.clone());
        Ok(())
    }

    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError> {
        self.map
            .read()
            .get(&(list.clone(), id))
            .cloned()
            .ok_or_else(|| TaskError::NotFound {
                list: list.clone(),
                id,
            })
    }

    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError> {
        self.map.write().remove(&(list.clone(), id));
        Ok(())
    }

    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError> {
        let mut out: Vec<Task> = self
            .map
            .read()
            .values()
            .filter(|t| t.list_id == *list && t.status != TaskStatus::Deleted)
            .cloned()
            .collect();
        out.sort_by_key(|t| t.id);
        Ok(out)
    }

    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError> {
        let mut hwm = self.hwm.write();
        let cur = hwm.get(list).copied().unwrap_or(0);
        let next = cur + 1;
        hwm.insert(list.clone(), next);
        Ok(next)
    }
}

// ── FileTaskStore(文件任务存储)────────────────────────────────

/// 文件后端。冷路径(重启 / 跨 session)使用,持久化到磁盘。
///
/// 路径:`<home>/tasks/<list>/<id>.json`(`<home>` 来自 `REFLECT_HOME`
/// 或 `$HOME/.reflect`)。`next_id` 与 `save` 用 list-level `Mutex` 串行化
/// 分配,高水位写回 `.highwatermark` 文件,避免跨进程 id 重复。
pub struct FileTaskStore {
    home: PathBuf,
    /// 每 list 一把 mutex,串行化 `next_id` 与 `save` 操作,避免
    /// 两个并发调用读到同一个高水位。
    list_locks: Mutex<HashMap<ListId, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for FileTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTaskStore")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl FileTaskStore {
    /// 用显式 `home` 构造(测试友好)。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            list_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 用默认 `$REFLECT_HOME` 或 `$HOME/.reflect` 构造。
    pub fn with_default_home() -> Result<Self, TaskError> {
        let home = std::env::var("REFLECT_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".reflect"))
            })
            .ok_or_else(|| TaskError::Invalid("REFLECT_HOME / HOME not set".into()))?;
        Ok(Self::new(home))
    }

    /// list-level 锁:同 list 的 `next_id` + `save` 串行执行。
    async fn lock_for(&self, list: &ListId) -> Arc<Mutex<()>> {
        let mut map = self.list_locks.lock().await;
        map.entry(list.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 读 list 下的高水位文件;不存在返回 0。
    fn read_hwm(&self, list_dir: &Path) -> TaskId {
        let p = list_dir.join(".highwatermark");
        match std::fs::read_to_string(&p) {
            Ok(s) => s.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
    }
}

#[async_trait]
impl TaskStore for FileTaskStore {
    fn dir_for(&self, list: &ListId) -> PathBuf {
        self.home.join("tasks").join(list)
    }

    async fn save(&self, list: &ListId, task: &Task) -> Result<(), TaskError> {
        let lock = self.lock_for(list).await;
        let _guard = lock.lock().await;
        let list_dir = self.dir_for(list);
        // 同步 I/O 放进 spawn_blocking 避免阻塞 tokio runtime。
        let dir = list_dir.clone();
        let task_for_write = task.clone();
        let result: Result<(), TaskError> =
            tokio::task::spawn_blocking(move || write_task_blocking(&dir, &task_for_write))
                .await
                .map_err(|e| TaskError::Invalid(format!("join error: {e}")))?;
        result?;
        // 同步提升高水位文件,避免 `next_id` 在外部直接写文件后读到
        // 过期的 max id。`next_id` 与 `save` 共用 list-level 锁,无竞态。
        let cur = self.read_hwm(&list_dir);
        if task.id > cur {
            let dir = list_dir.clone();
            let task_id = task.id;
            let hwm: Result<(), TaskError> =
                tokio::task::spawn_blocking(move || write_hwm_blocking(&dir, task_id))
                    .await
                    .map_err(|e| TaskError::Invalid(format!("join error: {e}")))?;
            hwm?;
        }
        Ok(())
    }

    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError> {
        let p = self.dir_for(list).join(format!("{id}.json"));
        let s = tokio::fs::read_to_string(&p)
            .await
            .map_err(|_| TaskError::NotFound {
                list: list.clone(),
                id,
            })?;
        let t: Task = serde_json::from_str(&s)?;
        Ok(t)
    }

    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError> {
        let p = self.dir_for(list).join(format!("{id}.json"));
        match tokio::fs::remove_file(&p).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TaskError::Io(e)),
        }
    }

    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError> {
        let dir = self.dir_for(list);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::Io(e)),
        };
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => match serde_json::from_str::<Task>(&s) {
                    Ok(t) if t.status != TaskStatus::Deleted => out.push(t),
                    Ok(_) => {} // skip soft-deleted
                    Err(e) => warn!(?path, error = %e, "skip malformed task json"),
                },
                Err(e) => warn!(?path, error = %e, "skip unreadable task json"),
            }
        }
        out.sort_by_key(|t| t.id);
        Ok(out)
    }

    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError> {
        let lock = self.lock_for(list).await;
        let _guard = lock.lock().await;
        let list_dir = self.dir_for(list);
        let cur = self.read_hwm(&list_dir);
        let next = cur + 1;
        let dir = list_dir.clone();
        let result: Result<(), TaskError> =
            tokio::task::spawn_blocking(move || write_hwm_blocking(&dir, next))
                .await
                .map_err(|e| TaskError::Invalid(format!("join error: {e}")))?;
        result?;
        Ok(next)
    }
}

fn write_task_blocking(dir: &Path, task: &Task) -> Result<(), TaskError> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(task)?;
    let tmp = dir.join(format!("{}.json.tmp", task.id));
    let final_p = dir.join(format!("{}.json", task.id));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, final_p)?;
    Ok(())
}

fn write_hwm_blocking(dir: &Path, id: TaskId) -> Result<(), TaskError> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".highwatermark.{id}.tmp"));
    let final_p = dir.join(".highwatermark");
    std::fs::write(&tmp, id.to_string())?;
    std::fs::rename(&tmp, final_p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn task(id: TaskId, list: &str, subject: &str) -> Task {
        let now = std::time::SystemTime::UNIX_EPOCH;
        Task {
            id,
            list_id: list.into(),
            subject: subject.into(),
            description: format!("desc for {subject}"),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: vec![],
            blocked_by: vec![],
            metadata: serde_json::json!({}),
            output_path: None,
            claimed_by: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn in_memory_save_load_roundtrip() {
        let s = InMemoryTaskStore::new();
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        let t = s.load(&"L".into(), 1).await.unwrap();
        assert_eq!(t.subject, "a");
    }

    #[tokio::test]
    async fn in_memory_load_missing_errors() {
        let s = InMemoryTaskStore::new();
        let err = s.load(&"L".into(), 99).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn in_memory_list_filters_deleted_and_sorts() {
        let s = InMemoryTaskStore::new();
        let mut t3 = task(3, "L", "c");
        t3.status = TaskStatus::Deleted;
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        s.save(&"L".into(), &task(2, "L", "b")).await.unwrap();
        s.save(&"L".into(), &t3).await.unwrap();
        let v = s.list(&"L".into()).await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 1);
        assert_eq!(v[1].id, 2);
    }

    #[tokio::test]
    async fn in_memory_next_id_increments_per_list() {
        let s = InMemoryTaskStore::new();
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 1);
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 2);
        assert_eq!(s.next_id(&"L2".into()).await.unwrap(), 1);
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn file_store_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "alpha")).await.unwrap();
        let t = s.load(&"L".into(), 1).await.unwrap();
        assert_eq!(t.subject, "alpha");
    }

    #[tokio::test]
    async fn file_store_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "x")).await.unwrap();
        assert!(dir.path().join("tasks/L/1.json").exists());
    }

    #[tokio::test]
    async fn file_store_load_missing_errors() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        let err = s.load(&"L".into(), 99).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn file_store_delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "x")).await.unwrap();
        s.delete(&"L".into(), 1).await.unwrap();
        let err = s.load(&"L".into(), 1).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn file_store_list_sorts_and_filters() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(2, "L", "b")).await.unwrap();
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        let mut t3 = task(3, "L", "c");
        t3.status = TaskStatus::Deleted;
        s.save(&"L".into(), &t3).await.unwrap();
        let v = s.list(&"L".into()).await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 1);
        assert_eq!(v[1].id, 2);
    }

    #[tokio::test]
    async fn file_store_next_id_persists_across_stores() {
        let dir = TempDir::new().unwrap();
        {
            let s = FileTaskStore::new(dir.path());
            assert_eq!(s.next_id(&"L".into()).await.unwrap(), 1);
            assert_eq!(s.next_id(&"L".into()).await.unwrap(), 2);
        }
        // 新建 store 读同一目录,高水位应恢复为 2。
        let s2 = FileTaskStore::new(dir.path());
        assert_eq!(s2.next_id(&"L".into()).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn file_store_lists_isolated_per_list() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L1".into(), &task(1, "L1", "a")).await.unwrap();
        s.save(&"L2".into(), &task(1, "L2", "x")).await.unwrap();
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 2);
        assert_eq!(s.next_id(&"L2".into()).await.unwrap(), 2);
    }
}
