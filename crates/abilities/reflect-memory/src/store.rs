//! `MemoryStore` trait + `FileMemoryStore` + `InMemoryStore`。
//!
//! 镜像 Reflect `agent_memory.py:AgentMemoryStore`(文件承载的 scope)
//! 加一个供 `Session` scope 使用的类 `dict` 内存 store。

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::model::{MemoryError, MemoryScope};
use crate::scope::{MAX_MEMORY_INJECT_CHARS, resolve_path};

/// 抽象 memory store。实现可持久化到磁盘、内存或两者。
/// 线程安全(`Send + Sync`)。
pub trait MemoryStore: Send + Sync {
    /// 读取指定 `(scope, agent_type)` 的 memory 正文。
    /// memory 尚不存在时返回空字符串。
    fn load(&self, scope: MemoryScope, agent_type: &str) -> Result<String, MemoryError>;

    /// 覆写 memory 正文。`Session` scope 下 [`InMemoryStore`]
    /// 更新其 map;[`FileMemoryStore`] 返回
    /// [`MemoryError::SessionNotPersisted`]。
    fn save(&self, scope: MemoryScope, agent_type: &str, content: &str) -> Result<(), MemoryError>;

    /// 按给定顺序读取并拼接请求的 scopes,用于注入 system prompt。
    /// 当非空 scope 多于一个时,每个非空 scope 的内容都以
    /// `### <scope> memory` 标题包裹。
    fn load_combined(
        &self,
        scopes: &[MemoryScope],
        agent_type: &str,
    ) -> Result<String, MemoryError> {
        let mut non_empty: Vec<(MemoryScope, String)> = Vec::new();
        for s in scopes {
            let body = self.load(*s, agent_type)?;
            if body.trim().is_empty() {
                continue;
            }
            non_empty.push((*s, body));
        }
        let mut parts = Vec::new();
        for (s, body) in &non_empty {
            if non_empty.len() > 1 {
                parts.push(format!("### {s} memory\n\n{}", body.trim()));
            } else {
                parts.push(body.clone());
            }
        }
        Ok(parts.join("\n\n"))
    }

    /// 列出某 `scope` 下所有已存在的 `agent_type`,按字典序返回。
    ///
    /// 用途:`/memory ls <scope>` 在 TUI 列出当前 scope 下的所有 memory。
    ///
    /// 默认实现返回 `Err(Invalid("list_agent_types not implemented"))`,
    /// 让只关心 `load/save/load_combined` 的最小实现不必关心此 API;
    /// [`FileMemoryStore`] / [`InMemoryStore`] / [`CompositeMemoryStore`]
    /// 均已实现。
    fn list_agent_types(&self, scope: MemoryScope) -> Result<Vec<String>, MemoryError> {
        let _ = scope;
        Err(MemoryError::Invalid(
            "list_agent_types not implemented for this MemoryStore".into(),
        ))
    }
}

/// 文件承载的 store。通过读写解析路径上的 `MEMORY.md` 处理
/// `Project` 与 `User` scope。`Session` scope 返回
/// [`MemoryError::SessionNotPersisted`]。
#[derive(Debug, Clone)]
pub struct FileMemoryStore {
    workspace: PathBuf,
    home: PathBuf,
}

impl FileMemoryStore {
    /// 新建 store。`home` 为 `None` 时默认 `$HOME`。
    pub fn new(workspace: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            home: home.into(),
        }
    }

    fn path(&self, scope: MemoryScope, agent_type: &str) -> Result<PathBuf, MemoryError> {
        resolve_path(scope, &self.workspace, &self.home, agent_type)
    }
}

impl MemoryStore for FileMemoryStore {
    fn load(&self, scope: MemoryScope, agent_type: &str) -> Result<String, MemoryError> {
        // Session scope 永不落盘;返回空且不报错。
        if matches!(scope, MemoryScope::Session) {
            return Ok(String::new());
        }
        let p = self.path(scope, agent_type)?;
        if !p.exists() {
            return Ok(String::new());
        }
        Ok(fs::read_to_string(&p)?)
    }

    fn save(&self, scope: MemoryScope, agent_type: &str, content: &str) -> Result<(), MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        let p = self.path(scope, agent_type)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&p)?;
        f.write_all(content.as_bytes())?;
        Ok(())
    }

    fn list_agent_types(&self, scope: MemoryScope) -> Result<Vec<String>, MemoryError> {
        // Session scope 无文件 —— 总是返回空。
        if matches!(scope, MemoryScope::Session) {
            return Ok(Vec::new());
        }
        let root = match scope {
            MemoryScope::Project => self.workspace.join(".reflect").join("agent-memory"),
            MemoryScope::User => self.home.join(".reflect").join("agent-memory"),
            MemoryScope::Session => unreachable!("guarded above"),
        };
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            // 只收叶子目录(每个 agent_type 一个目录)。
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !ft.is_dir() {
                continue;
            }
            // 必须含 MEMORY.md 才算有效;否则视为残留目录跳过。
            let memory_md = entry.path().join("MEMORY.md");
            if !memory_md.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

/// 内存 store。处理 `Session` scope;`Project` 与 `User` 可将其叠在
/// `FileMemoryStore` 之上作缓存层(由调用方组合;本 store 只持久化到
/// 内存)。
pub struct InMemoryStore {
    inner: RwLock<HashMap<String, String>>,
}

impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryStore")
            .field("entries", &self.inner.read().len())
            .finish()
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStore {
    /// 新建空 store。
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// 包裹一个 `FileMemoryStore`:内存 store 先查自身 map,
    /// `Project` / `User` 未命中时回退文件 store。
    /// `Session` 始终在内存。
    pub fn with_fallback(file: Arc<FileMemoryStore>) -> CompositeMemoryStore {
        CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file,
        }
    }

    fn key(scope: MemoryScope, agent_type: &str) -> String {
        format!("{scope}:{agent_type}")
    }

    /// 直接访问(`CompositeMemoryStore` 使用)。
    pub(crate) fn load_raw(&self, scope: MemoryScope, agent_type: &str) -> Option<String> {
        self.inner
            .read()
            .get(&Self::key(scope, agent_type))
            .cloned()
    }

    /// 直接访问。
    pub(crate) fn store_raw(&self, scope: MemoryScope, agent_type: &str, content: &str) {
        self.inner
            .write()
            .insert(Self::key(scope, agent_type), content.to_string());
    }
}

impl MemoryStore for InMemoryStore {
    fn load(&self, scope: MemoryScope, agent_type: &str) -> Result<String, MemoryError> {
        Ok(self
            .inner
            .read()
            .get(&Self::key(scope, agent_type))
            .cloned()
            .unwrap_or_default())
    }

    fn save(&self, scope: MemoryScope, agent_type: &str, content: &str) -> Result<(), MemoryError> {
        if !matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        self.store_raw(scope, agent_type, content);
        Ok(())
    }

    fn list_agent_types(&self, scope: MemoryScope) -> Result<Vec<String>, MemoryError> {
        // 扫描内部 map 里所有以 `<scope>:` 开头的 key —— 不论 InMemoryStore
        // 是否"允许"该 scope,key 都在 map 里(Composite 缓存层会替
        // Project / User 也存),所以直接扫前缀即可。
        let prefix = format!("{scope}:");
        let mut out: Vec<String> = self
            .inner
            .read()
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(|s| s.to_string()))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// 内存缓存 + 文件 store 的组合:读先查内存,未命中落到磁盘;
/// 写入时 `Session` 恒写内存,`Project` / `User` 恒写磁盘。
pub struct CompositeMemoryStore {
    pub memory: Arc<InMemoryStore>,
    pub file: Arc<FileMemoryStore>,
}

impl std::fmt::Debug for CompositeMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeMemoryStore")
            .finish_non_exhaustive()
    }
}

impl MemoryStore for CompositeMemoryStore {
    fn load(&self, scope: MemoryScope, agent_type: &str) -> Result<String, MemoryError> {
        if let Some(s) = self.memory.load_raw(scope, agent_type) {
            return Ok(s);
        }
        self.file.load(scope, agent_type)
    }

    fn save(&self, scope: MemoryScope, agent_type: &str, content: &str) -> Result<(), MemoryError> {
        match scope {
            MemoryScope::Session => self.memory.save(scope, agent_type, content),
            MemoryScope::Project | MemoryScope::User => {
                // 更新缓存 + 持久化。
                self.memory.store_raw(scope, agent_type, content);
                self.file.save(scope, agent_type, content)
            }
        }
    }

    fn list_agent_types(&self, scope: MemoryScope) -> Result<Vec<String>, MemoryError> {
        // 合并 memory + file 两层,按字典序去重返回。
        let mut out: Vec<String> = self.memory.list_agent_types(scope)?;
        out.extend(self.file.list_agent_types(scope)?);
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// 为注入截断 memory 正文。上限 [`MAX_MEMORY_INJECT_CHARS`],
/// 发生截断时追加标记。
pub fn truncate_for_injection(s: &str) -> String {
    if s.chars().count() <= MAX_MEMORY_INJECT_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_MEMORY_INJECT_CHARS).collect();
    format!("{truncated}\n...[memory truncated at {MAX_MEMORY_INJECT_CHARS} chars]...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn file_store_load_missing_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        let body = store.load(MemoryScope::Project, "default").unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn file_store_save_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        store
            .save(MemoryScope::Project, "default", "hello world")
            .unwrap();
        let body = store.load(MemoryScope::Project, "default").unwrap();
        assert_eq!(body, "hello world");
    }

    #[test]
    fn file_store_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        store.save(MemoryScope::Project, "deep_type", "x").unwrap();
        let expected = dir.path().join(".reflect/agent-memory/deep_type/MEMORY.md");
        assert!(expected.exists());
    }

    #[test]
    fn file_store_session_save_errors() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        let err = store
            .save(MemoryScope::Session, "default", "x")
            .unwrap_err();
        assert!(matches!(err, MemoryError::SessionNotPersisted));
    }

    #[test]
    fn in_memory_store_session_save_load() {
        let store = InMemoryStore::new();
        store
            .save(MemoryScope::Session, "default", "sess body")
            .unwrap();
        assert_eq!(
            store.load(MemoryScope::Session, "default").unwrap(),
            "sess body"
        );
    }

    #[test]
    fn in_memory_store_project_save_errors() {
        let store = InMemoryStore::new();
        let err = store
            .save(MemoryScope::Project, "default", "x")
            .unwrap_err();
        assert!(matches!(err, MemoryError::SessionNotPersisted));
    }

    #[test]
    fn composite_reads_through_to_file() {
        let dir = TempDir::new().unwrap();
        let file = Arc::new(FileMemoryStore::new(dir.path(), "/home"));
        let composite = CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file: file.clone(),
        };
        file.save(MemoryScope::Project, "default", "from disk")
            .unwrap();
        assert_eq!(
            composite.load(MemoryScope::Project, "default").unwrap(),
            "from disk"
        );
    }

    #[test]
    fn composite_writes_to_both_layers_for_project() {
        let dir = TempDir::new().unwrap();
        let file = Arc::new(FileMemoryStore::new(dir.path(), "/home"));
        let composite = CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file: file.clone(),
        };
        composite
            .save(MemoryScope::Project, "default", "fresh")
            .unwrap();
        // 两层都有。
        assert_eq!(
            composite
                .memory
                .load(MemoryScope::Project, "default")
                .unwrap(),
            "fresh"
        );
        assert_eq!(file.load(MemoryScope::Project, "default").unwrap(), "fresh");
    }

    #[test]
    fn composite_session_only_writes_to_memory() {
        let dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let file = Arc::new(FileMemoryStore::new(dir.path(), home.path()));
        let composite = CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file: file.clone(),
        };
        composite
            .save(MemoryScope::Session, "default", "ephemeral")
            .unwrap();
        assert_eq!(
            composite.load(MemoryScope::Session, "default").unwrap(),
            "ephemeral"
        );
        // 文件 store 对 Session scope 返回空(无文件路径)。
        assert_eq!(file.load(MemoryScope::Session, "default").unwrap(), "");
    }

    #[test]
    fn load_combined_skips_empty_scopes() {
        let dir = TempDir::new().unwrap();
        let file = FileMemoryStore::new(dir.path(), "/home");
        file.save(MemoryScope::Project, "default", "proj body")
            .unwrap();
        // User 为空。
        let combined = file
            .load_combined(&[MemoryScope::Project, MemoryScope::User], "default")
            .unwrap();
        assert_eq!(combined, "proj body");
    }

    #[test]
    fn load_combined_with_headers_when_multiple() {
        let dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let file = FileMemoryStore::new(dir.path(), home.path());
        file.save(MemoryScope::Project, "default", "proj").unwrap();
        file.save(MemoryScope::User, "default", "user").unwrap();
        let combined = file
            .load_combined(&[MemoryScope::Project, MemoryScope::User], "default")
            .unwrap();
        assert!(combined.contains("### project memory"));
        assert!(combined.contains("### user memory"));
        assert!(combined.contains("proj"));
        assert!(combined.contains("user"));
    }

    #[test]
    fn truncate_under_cap_unchanged() {
        let s = "x".repeat(100);
        assert_eq!(truncate_for_injection(&s), s);
    }

    #[test]
    fn truncate_over_cap_appends_marker() {
        let s = "x".repeat(MAX_MEMORY_INJECT_CHARS + 100);
        let out = truncate_for_injection(&s);
        assert!(out.contains("[memory truncated"));
        assert!(out.chars().count() <= MAX_MEMORY_INJECT_CHARS + 200);
    }

    // ── 列出 agent 类型(S2.1)───────────────────────────────

    /// File 后端:扫 `agent-memory/<agent_type>/MEMORY.md`,按字典序返回,
    /// 只收含 MEMORY.md 的目录(残留空目录跳过)。
    #[test]
    fn file_list_agent_types_scans_and_sorts() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        store.save(MemoryScope::Project, "alpha", "a-body").unwrap();
        store.save(MemoryScope::Project, "beta", "b-body").unwrap();
        store.save(MemoryScope::Project, "gamma", "g-body").unwrap();
        let list = store.list_agent_types(MemoryScope::Project).unwrap();
        assert_eq!(list, vec!["alpha", "beta", "gamma"]);
    }

    /// File 后端:目录不存在时返回空 vec(不报错)。
    #[test]
    fn file_list_agent_types_empty_when_no_root() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        let list = store.list_agent_types(MemoryScope::User).unwrap();
        assert!(list.is_empty());
    }

    /// File 后端:残留空目录(无 MEMORY.md)跳过,不计入列表。
    #[test]
    fn file_list_agent_types_skips_dirs_without_memory_md() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        store.save(MemoryScope::Project, "real", "body").unwrap();
        // 手动建一个无 MEMORY.md 的目录
        std::fs::create_dir_all(
            dir.path()
                .join(".reflect")
                .join("agent-memory")
                .join("ghost"),
        )
        .unwrap();
        let list = store.list_agent_types(MemoryScope::Project).unwrap();
        assert_eq!(list, vec!["real"]);
    }

    /// File 后端:Session scope 永远返回空(Session 不落盘)。
    #[test]
    fn file_list_agent_types_session_is_empty() {
        let dir = TempDir::new().unwrap();
        let store = FileMemoryStore::new(dir.path(), "/home");
        let list = store.list_agent_types(MemoryScope::Session).unwrap();
        assert!(list.is_empty());
    }

    /// InMemory 后端:Session scope 解析 key 前缀 `session:<agent>`。
    #[test]
    fn in_memory_list_agent_types_session_parses_keys() {
        let store = InMemoryStore::new();
        store.save(MemoryScope::Session, "x", "1").unwrap();
        store.save(MemoryScope::Session, "y", "2").unwrap();
        store.save(MemoryScope::Session, "z", "3").unwrap();
        let list = store.list_agent_types(MemoryScope::Session).unwrap();
        assert_eq!(list, vec!["x", "y", "z"]);
    }

    /// InMemory 后端:key 前缀扫描只对请求的 scope 生效 —— 跨 scope 隔离。
    #[test]
    fn in_memory_list_agent_types_scopes_are_isolated() {
        let store = InMemoryStore::new();
        // 只允许 Session scope save;Project / User 必须用 store_raw。
        store.save(MemoryScope::Session, "sess-only", "x").unwrap();
        store.store_raw(MemoryScope::Project, "proj-only", "y");
        store.store_raw(MemoryScope::User, "user-only", "z");
        let sess = store.list_agent_types(MemoryScope::Session).unwrap();
        let proj = store.list_agent_types(MemoryScope::Project).unwrap();
        let user = store.list_agent_types(MemoryScope::User).unwrap();
        assert_eq!(sess, vec!["sess-only"]);
        assert_eq!(proj, vec!["proj-only"]);
        assert_eq!(user, vec!["user-only"]);
    }

    /// Composite 后端:合并 memory + file 两层,去重排序。
    #[test]
    fn composite_list_agent_types_merges_and_dedups() {
        let dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let file = Arc::new(FileMemoryStore::new(dir.path(), home.path()));
        let composite = CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file: file.clone(),
        };
        // file 层有 alpha + beta
        file.save(MemoryScope::Project, "alpha", "a").unwrap();
        file.save(MemoryScope::Project, "beta", "b").unwrap();
        // memory 层加了 gamma + 与 file 重叠的 alpha
        // 注意:`InMemoryStore::save` 对 Project scope 报错,
        // 必须用 `store_raw`(pub(crate))直接注入底层 map。
        composite
            .memory
            .store_raw(MemoryScope::Project, "gamma", "g");
        composite
            .memory
            .store_raw(MemoryScope::Project, "alpha", "a-dup");
        let list = composite.list_agent_types(MemoryScope::Project).unwrap();
        assert_eq!(list, vec!["alpha", "beta", "gamma"]);
    }

    /// Composite 后端:Session 走 memory 层解析;Project 同时合并两层。
    #[test]
    fn composite_list_agent_types_session_only_memory() {
        let dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let file = Arc::new(FileMemoryStore::new(dir.path(), home.path()));
        let composite = CompositeMemoryStore {
            memory: Arc::new(InMemoryStore::new()),
            file: file.clone(),
        };
        composite
            .memory
            .save(MemoryScope::Session, "s1", "x")
            .unwrap();
        let list = composite.list_agent_types(MemoryScope::Session).unwrap();
        assert_eq!(list, vec!["s1"]);
        // file 层对 Session 永远空,所以不参与合并。
        let proj_list = composite.list_agent_types(MemoryScope::Project).unwrap();
        assert!(proj_list.is_empty());
    }
}
