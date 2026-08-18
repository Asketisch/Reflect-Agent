//! `store` —— PermissionRule 持久化(TOML 文件 + 内存实现)。
//!
//! 格式: `[[rule]]` 数组,每条 `{ tool, action }`。
//!
//! ```toml
//! # ~/.reflect/permissions.toml        # 权限规则文件路径
//! [[rule]]                             # 规则数组的第一条
//! tool = "Bash"                        # 工具名
//! action = "allow"                     # 动作:allow / deny / ask
//!
//! [[rule]]                             # 规则数组的第二条
//! tool = "Write"                       # 工具名
//! action = "deny"                      # 动作:allow / deny / ask
//! ```
//!
//! v1.x 简化:无 lock / 并发写。如果并发 TUI + CLI 同时 add,最后写者
//! 覆盖(同 `reflect-rollout::path` 风格)。`add` 内部 read-modify-write
//! 保证已有 rules 不丢。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::rules::PermissionRule;

#[cfg(test)]
use crate::rules::PermissionAction;

/// 持久化层错误。区分 IO / parse / env(无 HOME),让 caller 决定怎么
/// 反馈给用户。
#[derive(Debug, Error)]
pub enum PermissionsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialize toml: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("HOME env var not set")]
    NoHome,
}

/// File format wrapper —— `[[rule]]` 数组的根对象。
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PermissionFile {
    #[serde(default, rename = "rule")]
    rules: Vec<PermissionRule>,
}

/// Permission store trait。`async` 简化未来扩展(网络同步、数据库),v1.x
/// 两个 impl 都是 sync 但 wrap 成 async。
#[async_trait::async_trait]
pub trait PermissionStore: Send + Sync {
    /// 列出全部规则。**不**排序 —— caller 决定展示顺序。
    async fn list(&self) -> Result<Vec<PermissionRule>, PermissionsError>;
    /// 添加一条规则(append 到末尾)。已存在的同名 tool **不**替换 —
    /// 保留"first match wins"语义,`evaluate` 文档要求。
    async fn add(&self, rule: PermissionRule) -> Result<(), PermissionsError>;
    /// 按 tool 名删一条。找不到 → `Ok(())`(幂等,add/remove 配对更友好)。
    async fn remove(&self, tool: &str) -> Result<(), PermissionsError>;
}

// ── InMemoryPermissionStore(内存权限存储)──────────────────────────────

/// 内存实现,测试用,无 IO。
#[derive(Debug, Default)]
pub struct InMemoryPermissionStore {
    rules: Mutex<Vec<PermissionRule>>,
}

impl InMemoryPermissionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl PermissionStore for InMemoryPermissionStore {
    async fn list(&self) -> Result<Vec<PermissionRule>, PermissionsError> {
        Ok(self.rules.lock().clone())
    }

    async fn add(&self, rule: PermissionRule) -> Result<(), PermissionsError> {
        self.rules.lock().push(rule);
        Ok(())
    }

    async fn remove(&self, tool: &str) -> Result<(), PermissionsError> {
        let mut g = self.rules.lock();
        g.retain(|r| r.tool != tool);
        Ok(())
    }
}

// ── ChainedPermissionStore(链式权限存储)──────────────────────────────

/// 组合多个 store,`list` 时把所有 store 的规则**拼接**(前面的 store
/// 优先,保证 first-match-wins 时靠前的 store 规则先生效)。
///
/// 主要用途:把 config.toml 的 `[permissions]` 规则(InMemory,启动期灌入,
/// 只读)与 `~/.reflect/permissions.toml` 规则(FilePermissionStore,可写)
/// 组合成单一 store 喂给 `StorePermissionResolver`。config 规则在**前**(优先)。
///
/// `add` / `remove` 委托给**最后一个** store(可写层)—— 这样 `/permissions
/// allow` 命令写的规则落到可写层(通常是 file store),持久化到磁盘,
/// 不污染只读的 config 规则层。`list` 顺序(config 在前)与可写层(file 在后)
/// 分离,既保证 config 优先匹配,又让运行时写入落盘。
pub struct ChainedPermissionStore {
    stores: Vec<Arc<dyn PermissionStore>>,
}

// 手写 Debug:`Arc<dyn PermissionStore>` 没有 derive Debug(`dyn` 无 Debug bound)。
impl std::fmt::Debug for ChainedPermissionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainedPermissionStore")
            .field("stores", &self.stores.len())
            .finish()
    }
}

impl ChainedPermissionStore {
    /// 构造链式 store。
    ///
    /// **顺序约定(与 `list` / `add` / `remove` 的实际行为对齐)**:
    /// - `list` 把 `stores[0], stores[1], …, stores[n-1]` 的规则**按序拼接**。
    ///   `evaluate` 走 first-match-wins,所以**靠前的 store 优先匹配**。
    ///   典型安排:只读层(config rules)在前、可写层(file store)在后 →
    ///   config 规则优先匹配。
    /// - `add` / `remove` **无条件委托给 `stores[n-1]`(最后一个 store)**。
    ///   因此**最后一个 store 必须是可写层**(InMemory 或 FilePermissionStore)。
    ///   前面的 store 通常是只读的,不被 `add`/`remove` 触及。
    ///
    /// 至少需要一个 store;空 vec 会 panic(构造期错误,调用方应保证非空)。
    pub fn new(stores: Vec<Arc<dyn PermissionStore>>) -> Self {
        assert!(
            !stores.is_empty(),
            "ChainedPermissionStore requires at least one store"
        );
        Self { stores }
    }
}

#[async_trait::async_trait]
impl PermissionStore for ChainedPermissionStore {
    async fn list(&self) -> Result<Vec<PermissionRule>, PermissionsError> {
        // 预估容量,避免多次扩容。
        let mut all: Vec<PermissionRule> = Vec::new();
        for s in &self.stores {
            all.extend(s.list().await?);
        }
        Ok(all)
    }

    async fn add(&self, rule: PermissionRule) -> Result<(), PermissionsError> {
        // 委托给最后一个 store(可写层 —— 通常是要落盘的 file store)。
        // 注意:list 顺序(config 在前)与可写层(file 在后)分离。
        self.stores[self.stores.len() - 1].add(rule).await
    }

    async fn remove(&self, tool: &str) -> Result<(), PermissionsError> {
        // 委托给最后一个 store(可写层)。
        self.stores[self.stores.len() - 1].remove(tool).await
    }
}

// ── FilePermissionStore(文件权限存储)────────────────────────────────

/// TOML 文件持久化,默认路径 `~/.reflect/permissions.toml`。
#[derive(Debug)]
pub struct FilePermissionStore {
    path: PathBuf,
}

impl FilePermissionStore {
    /// 用 `~/.reflect/permissions.toml`(HOME 不存在 → `Err(NoHome)`)。
    pub fn with_default_home() -> Result<Self, PermissionsError> {
        let home = std::env::var_os("HOME").ok_or(PermissionsError::NoHome)?;
        Ok(Self::with_path(
            PathBuf::from(home).join(".reflect/permissions.toml"),
        ))
    }

    /// 测试用 —— 直接给路径,跳过 HOME env 依赖。
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// 暴露路径,给 Pill 输出提示用(让用户知道 rules 存在哪)。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 同步读 + 解析。文件不存在 → `Ok(default())`(让 `list` 起点是空,
    /// 不是 `Err`)—— 与 `reflect-rollout` 的"缺失 base 返回空"风格一致。
    fn read(&self) -> Result<PermissionFile, PermissionsError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => {
                if s.trim().is_empty() {
                    Ok(PermissionFile::default())
                } else {
                    Ok(toml::from_str(&s)?)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PermissionFile::default()),
            Err(e) => Err(PermissionsError::Io(e)),
        }
    }

    /// 同步写:保证 parent dir 存在(避免 PermissionDenied on first write),
    /// 然后 toml::to_string_pretty + write。
    fn write(&self, file: &PermissionFile) -> Result<(), PermissionsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(file)?;
        std::fs::write(&self.path, body)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PermissionStore for FilePermissionStore {
    async fn list(&self) -> Result<Vec<PermissionRule>, PermissionsError> {
        Ok(self.read()?.rules)
    }

    async fn add(&self, rule: PermissionRule) -> Result<(), PermissionsError> {
        let mut file = self.read()?;
        // "first match wins" + 用户 add 顺序隐式定 precedence → 不去重
        // 同名(tool 一致 + action 不同也算合法,后写的在尾部不影响 evaluate)。
        // 但完全相同(tool + action)就跳过,避免无意义重复。
        if !file.rules.iter().any(|r| r == &rule) {
            file.rules.push(rule);
            self.write(&file)?;
        }
        Ok(())
    }

    async fn remove(&self, tool: &str) -> Result<(), PermissionsError> {
        let mut file = self.read()?;
        let before = file.rules.len();
        file.rules.retain(|r| r.tool != tool);
        if file.rules.len() != before {
            self.write(&file)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rule(tool: &str, action: PermissionAction) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            action,
            tool_glob: None,
            shell_pattern: None,
        }
    }

    #[tokio::test]
    async fn in_memory_add_list_remove_round_trip() {
        let s = InMemoryPermissionStore::new();
        assert!(s.list().await.unwrap().is_empty());
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        s.add(rule("Write", PermissionAction::Deny)).await.unwrap();
        let rules = s.list().await.unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], rule("Bash", PermissionAction::Allow));
        assert_eq!(rules[1], rule("Write", PermissionAction::Deny));
        s.remove("Bash").await.unwrap();
        let after = s.list().await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].tool, "Write");
        // 不存在的 tool → 幂等,Ok(())。
        s.remove("Nope").await.unwrap();
    }

    #[tokio::test]
    async fn file_store_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("perms.toml");
        let s = FilePermissionStore::with_path(p);
        assert!(s.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn file_store_add_persists_round_trip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("perms.toml");
        let s = FilePermissionStore::with_path(p.clone());
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        s.add(rule("Read", PermissionAction::Ask)).await.unwrap();
        // 新建同路径 store,验证磁盘持久化。
        let s2 = FilePermissionStore::with_path(p);
        let rules = s2.list().await.unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], rule("Bash", PermissionAction::Allow));
        assert_eq!(rules[1], rule("Read", PermissionAction::Ask));
    }

    #[tokio::test]
    async fn file_store_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nested").join("perms.toml");
        let s = FilePermissionStore::with_path(p);
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        // 父目录被自动创建,文件存在。
        assert!(s.path().exists());
    }

    #[tokio::test]
    async fn file_store_add_duplicate_skips() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("perms.toml");
        let s = FilePermissionStore::with_path(p);
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        // 完全重复(tool + action)不写,避免无意义重复。
        assert_eq!(s.list().await.unwrap().len(), 1);
        // 但不同 action 算合法 —— first match wins,后写的在尾不影响 evaluate。
        s.add(rule("Bash", PermissionAction::Deny)).await.unwrap();
        assert_eq!(s.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn file_store_remove_only_affects_matching_tool() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("perms.toml");
        let s = FilePermissionStore::with_path(p);
        s.add(rule("Bash", PermissionAction::Allow)).await.unwrap();
        s.add(rule("Write", PermissionAction::Deny)).await.unwrap();
        s.remove("Bash").await.unwrap();
        let after = s.list().await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].tool, "Write");
    }

    #[tokio::test]
    async fn file_store_handles_corrupt_toml() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("perms.toml");
        std::fs::write(&p, "not = valid toml :::").unwrap();
        let s = FilePermissionStore::with_path(p);
        // 解析失败 → Err(Parse),让上层决定 fallback(当前 resolver 把
        // 错误降级为 NoMatch)。
        let r = s.list().await;
        assert!(r.is_err());
    }

    // ── ChainedPermissionStore(链式权限存储)──

    #[tokio::test]
    async fn chained_list_concatenates_all_stores_in_order() {
        // 前面的 store 规则排在 list 结果前面(保证 first-match-wins 优先级)。
        let a: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        let b: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        a.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
        })
        .await
        .unwrap();
        b.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: None,
        })
        .await
        .unwrap();
        let chained = ChainedPermissionStore::new(vec![a, b]);
        let rules = chained.list().await.unwrap();
        assert_eq!(rules.len(), 2, "应拼接 2 条规则");
        // a(Allow git *)在前,b(Deny)在后 → first-match-wins 时 Allow 优先。
        assert_eq!(rules[0].shell_pattern.as_deref(), Some("git *"));
        assert_eq!(rules[1].action, PermissionAction::Deny);
    }

    #[tokio::test]
    async fn chained_add_remove_delegate_to_last_store() {
        // add/remove 只影响最后一个 store(可写层),不污染只读层(前面的)。
        // 语义:config 规则(InMemory,只读)在前,file store(可写)在后;
        // /permissions allow 命令应落盘到 file store,而非污染 config 层。
        let a: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new()); // 只读(config)
        let b: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new()); // 可写(file)
        b.add(PermissionRule {
            tool: "Write".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: None,
        })
        .await
        .unwrap();
        let chained = ChainedPermissionStore::new(vec![a.clone(), b.clone()]);
        chained
            .add(PermissionRule {
                tool: "Bash".into(),
                action: PermissionAction::Allow,
                tool_glob: None,
                shell_pattern: None,
            })
            .await
            .unwrap();
        // add 落到 b(最后一个,可写层),a(只读层)不变。
        assert!(
            a.list().await.unwrap().is_empty(),
            "只读层(前)不应被 add 污染"
        );
        assert_eq!(b.list().await.unwrap().len(), 2, "可写层(后)应收到 add");
        // remove 也只作用于最后一个 store。
        chained.remove("Bash").await.unwrap();
        assert!(
            a.list().await.unwrap().is_empty(),
            "只读层(前)不受 remove 影响"
        );
        assert_eq!(
            b.list().await.unwrap().len(),
            1,
            "可写层(后)remove 应删掉 Bash 那条,剩 Write"
        );
    }
}
