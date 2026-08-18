//! `TeamStore` trait + `InMemoryTeamStore` + `FileTeamStore`。
//!
//! 与 [`crate::store::TaskStore`] 同构,但管理 `TeamFile`(整个 team 一个
//! 文件,不是每个成员一个文件)。路径:`<home>/teams/<name>.json`。
//!
//! 跨进程锁:Phase 2 落地 `TeamCreate` / `TeamDelete` 时,需要在
//! `upsert_team` / `delete_team` 上加 advisory lock(避免两个并发写者
//! 互相覆盖)。Phase 0 只做基础 CRUD,锁由 `TaskManager` 在 Phase 2 引入。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::error::TaskError;
use crate::model::{TeamFile, TeamName};

/// Team 存储后端。`Send + Sync`。
#[async_trait]
pub trait TeamStore: Send + Sync {
    /// 团队配置文件路径(只读暴露,用于 `output_path` 等派生)。
    fn path_for(&self, name: &TeamName) -> PathBuf;

    /// 读取单个 team;不存在返回 `TaskError::TeamNotFound`。
    async fn load(&self, name: &TeamName) -> Result<TeamFile, TaskError>;

    /// 保存 team(`tmp + rename` 原子写)。
    async fn save(&self, team: &TeamFile) -> Result<(), TaskError>;

    /// 删除 team 文件。
    async fn delete(&self, name: &TeamName) -> Result<(), TaskError>;

    /// 列出所有 team 名字(字典序)。
    async fn list_names(&self) -> Result<Vec<TeamName>, TaskError>;
}

// ── InMemoryTeamStore(内存团队存储)──────────────────────────

/// 内存后端。用 `parking_lot::RwLock<HashMap<TeamName, TeamFile>>` 容纳数据。
pub struct InMemoryTeamStore {
    map: RwLock<HashMap<TeamName, TeamFile>>,
}

impl std::fmt::Debug for InMemoryTeamStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryTeamStore")
            .field("count", &self.map.read().len())
            .finish()
    }
}

impl Default for InMemoryTeamStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTeamStore {
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl TeamStore for InMemoryTeamStore {
    fn path_for(&self, name: &TeamName) -> PathBuf {
        PathBuf::from(format!("<in-memory>/{name}.json"))
    }

    async fn load(&self, name: &TeamName) -> Result<TeamFile, TaskError> {
        self.map
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| TaskError::TeamNotFound(name.clone()))
    }

    async fn save(&self, team: &TeamFile) -> Result<(), TaskError> {
        self.map.write().insert(team.name.clone(), team.clone());
        Ok(())
    }

    async fn delete(&self, name: &TeamName) -> Result<(), TaskError> {
        self.map.write().remove(name);
        Ok(())
    }

    async fn list_names(&self) -> Result<Vec<TeamName>, TaskError> {
        let mut names: Vec<TeamName> = self.map.read().keys().cloned().collect();
        names.sort();
        Ok(names)
    }
}

// ── FileTeamStore(文件团队存储)────────────────────────────────

/// 文件后端。路径:`<home>/teams/<name>.json`(`<home>` 来自 `REFLECT_HOME`
/// 或 `$HOME/.reflect`)。
///
/// 并发安全:每 team 一把 `tokio::Mutex`(`team_locks` 字段),`save` / `delete`
/// 持锁后写盘,避免两个并发 `TeamCreate` 互相覆盖。沿用 `FileTaskStore::list_locks`
/// 同样的 pattern —— 跨进程并发通过 per-team lock 串行化,跨进程 race
/// 退化为"`Locked` 错误 + LLM 重试"(极低频,Phase 2 不专门优化)。
pub struct FileTeamStore {
    home: PathBuf,
    /// 每 team 一把 mutex,串行化 `save` / `delete` / `load`(只读取共享锁)。
    team_locks: Mutex<HashMap<TeamName, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for FileTeamStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTeamStore")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl FileTeamStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            team_locks: Mutex::new(HashMap::new()),
        }
    }

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

    fn dir(&self) -> PathBuf {
        self.home.join("teams")
    }

    /// 取/创建指定 team 的 mutex。`save` / `delete` 取独占,`load` 取共享锁
    /// 由调用方决定。
    async fn lock_for(&self, name: &TeamName) -> Arc<Mutex<()>> {
        let mut map = self.team_locks.lock().await;
        map.entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[async_trait]
impl TeamStore for FileTeamStore {
    fn path_for(&self, name: &TeamName) -> PathBuf {
        self.dir().join(format!("{name}.json"))
    }

    async fn load(&self, name: &TeamName) -> Result<TeamFile, TaskError> {
        // load 不强求独占锁 —— 读取期间的并发 save 写到 tmp + rename 是
        // 原子操作,读到的要么是旧版要么是新版,不会读到中间态。
        // 取 lock 仅用于与同名 delete 互斥(避免读到被删除的文件)。
        let lock = self.lock_for(name).await;
        let _guard = lock.lock().await;
        let p = self.path_for(name);
        let s = tokio::fs::read_to_string(&p)
            .await
            .map_err(|_| TaskError::TeamNotFound(name.clone()))?;
        let t: TeamFile = serde_json::from_str(&s)?;
        Ok(t)
    }

    async fn save(&self, team: &TeamFile) -> Result<(), TaskError> {
        let lock = self.lock_for(&team.name).await;
        let _guard = lock.lock().await;
        let dir = self.dir();
        let team = team.clone();
        let result: Result<(), TaskError> = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            let json = serde_json::to_string_pretty(&team)?;
            let tmp = dir.join(format!("{}.json.tmp", team.name));
            let final_p = dir.join(format!("{}.json", team.name));
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, final_p)?;
            Ok(())
        })
        .await
        .map_err(|e| TaskError::Invalid(format!("join error: {e}")))?;
        result
    }

    async fn delete(&self, name: &TeamName) -> Result<(), TaskError> {
        let lock = self.lock_for(name).await;
        let _guard = lock.lock().await;
        let p = self.path_for(name);
        match tokio::fs::remove_file(&p).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TaskError::Io(e)),
        }
    }

    async fn list_names(&self) -> Result<Vec<TeamName>, TaskError> {
        // list 不需 per-team 锁 —— 仅遍历目录,获取文件名;并发 save/delete
        // 期间可能临时看到 `*.json.tmp` 或刚被删除的文件,通过后缀过滤。
        let dir = self.dir();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::Io(e)),
        };
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            if let Some(stem) = name.strip_suffix(".json") {
                names.push(stem.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::model::TeamMemberSpec;

    fn team(name: &str, members: usize) -> TeamFile {
        let now = std::time::SystemTime::UNIX_EPOCH;
        let lead = crate::team::lead_agent_id_for(name);
        let mut ms = Vec::with_capacity(members);
        for i in 0..members {
            ms.push(TeamMemberSpec {
                agent_id: format!("member-{i}@{name}"),
                name: format!("m{i}"),
                role: format!("m{i}"),
                model: None,
                system_prompt: String::new(),
                allowed_tools: vec![],
                color: None,
                joined_at: now,
                session_id: None,
                subscriptions: vec![],
            });
        }
        TeamFile {
            name: name.into(),
            description: Some(format!("{name} team")),
            lead_agent_id: lead,
            lead_session_id: None,
            members: ms,
            created_at: now,
        }
    }

    #[tokio::test]
    async fn in_memory_save_load_roundtrip() {
        let s = InMemoryTeamStore::new();
        let t = team("rocket", 2);
        s.save(&t).await.unwrap();
        let back = s.load(&"rocket".into()).await.unwrap();
        assert_eq!(back.name, "rocket");
        assert_eq!(back.members.len(), 2);
    }

    #[tokio::test]
    async fn in_memory_load_missing_errors() {
        let s = InMemoryTeamStore::new();
        let err = s.load(&"ghost".into()).await.unwrap_err();
        assert!(matches!(err, TaskError::TeamNotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_list_names_sorted() {
        let s = InMemoryTeamStore::new();
        s.save(&team("zulu", 0)).await.unwrap();
        s.save(&team("alpha", 0)).await.unwrap();
        s.save(&team("mike", 0)).await.unwrap();
        let names = s.list_names().await.unwrap();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    #[tokio::test]
    async fn file_store_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let s = FileTeamStore::new(dir.path());
        let t = team("rocket", 2);
        s.save(&t).await.unwrap();
        assert!(dir.path().join("teams/rocket.json").exists());
        let back = s.load(&"rocket".into()).await.unwrap();
        assert_eq!(back.members.len(), 2);
        assert_eq!(back.lead_agent_id, "team-lead@rocket");
    }

    #[tokio::test]
    async fn file_store_delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let s = FileTeamStore::new(dir.path());
        s.save(&team("rocket", 0)).await.unwrap();
        s.delete(&"rocket".into()).await.unwrap();
        let err = s.load(&"rocket".into()).await.unwrap_err();
        assert!(matches!(err, TaskError::TeamNotFound(_)));
    }

    #[tokio::test]
    async fn file_store_list_names_sorted() {
        let dir = TempDir::new().unwrap();
        let s = FileTeamStore::new(dir.path());
        s.save(&team("zulu", 0)).await.unwrap();
        s.save(&team("alpha", 0)).await.unwrap();
        s.save(&team("mike", 0)).await.unwrap();
        let names = s.list_names().await.unwrap();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    #[tokio::test]
    async fn file_store_concurrent_save_same_team_serializes() {
        // 两个并发 save 同一 team 应串行化 —— 最终文件内容是某一个完整
        // 版本,不能读到中间态(`*.json.tmp` 已 rename 完毕)。
        let dir = TempDir::new().unwrap();
        let s = Arc::new(FileTeamStore::new(dir.path()));

        let mut a = team("rocket", 1);
        a.description = Some("version A".into());
        let mut b = team("rocket", 1);
        b.description = Some("version B".into());

        let sa = s.clone();
        let sb = s.clone();
        let (ra, rb) = tokio::join!(sa.save(&a), sb.save(&b));
        ra.unwrap();
        rb.unwrap();

        let back = s.load(&"rocket".into()).await.unwrap();
        // 任一完整版本都接受;关键是字段非空且 = A 或 B。
        assert!(
            back.description.as_deref() == Some("version A")
                || back.description.as_deref() == Some("version B"),
            "expected one of two complete versions, got {:?}",
            back.description
        );
        // 不存在 .tmp 残留
        let entries = std::fs::read_dir(dir.path().join("teams")).unwrap();
        for e in entries {
            let name = e.unwrap().file_name();
            let s = name.to_string_lossy();
            assert!(!s.ends_with(".tmp"), "stale tmp file: {s}");
        }
    }

    #[tokio::test]
    async fn file_store_concurrent_save_delete_race() {
        // 交叉执行 save + delete 同一 team:不应 panic,最终文件要么存在
        // 要么不存在,不会半写。
        let dir = TempDir::new().unwrap();
        let s = Arc::new(FileTeamStore::new(dir.path()));

        let mut handles = Vec::new();
        for i in 0..8 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                let mut t = team("rocket", 0);
                t.description = Some(format!("v{i}"));
                if i % 2 == 0 {
                    s.save(&t).await.unwrap();
                } else {
                    s.delete(&"rocket".into()).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // 不论最终状态,都不应有 .tmp 残留
        if let Ok(entries) = std::fs::read_dir(dir.path().join("teams")) {
            for e in entries {
                let name = e.unwrap().file_name();
                let s = name.to_string_lossy();
                assert!(!s.ends_with(".tmp"), "stale tmp file: {s}");
            }
        }
    }
}
