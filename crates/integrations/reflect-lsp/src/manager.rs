//! LSP 连接生命周期管理:`start_server` / `reload` / `shutdown`。
//!
//! 镜像 `reflect_mcp::manager::McpConnectionManager` 的结构,但去掉了
//! HTTP 路径(LSP 几乎都走 stdio)。
//!
//! ## 启动流程
//!
//! 1. `LspPeer::spawn_with_caps(...)` 启子进程 + 跑 `initialize` 握手,
//!    拿到 `ServerCapabilities` + `ServerInfo`。
//! 2. 构造 `LspClientInner`(持有 peer + docs map + cancel token)。
//! 3. 插 `handles: HashMap<name, LspServerHandle>`。
//! 4. 推 `LspLifecycleEvent::Started { server, methods, language_ids }`。
//!
//! ## 关闭流程(`stop_server`)
//!
//! 1. 从 handles 移除 entry。
//! 2. `close_all` 把已 open 文档批量 `didClose`(best-effort)。
//! 3. `inner.cancel.cancel()` → reader/writer task 退出 → child drop →
//!    `kill_on_drop` 杀子进程。
//! 4. 推 `LspLifecycleEvent::Stopped`。
//!
//! ## 重载流程(`reload`)
//!
//! Phase A 不做 hot reload,仅返回 diff 报告;Phase B 接入 `handle_reload`。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};

use crate::LspServerConfig;
use crate::client::LspClientInner;
use crate::diagnostics::DiagnosticRegistry;
use crate::error::LspError;
use crate::transport::LspPeer;

/// LSP server 在 manager 中的 entry。
///
/// 持有 client + 元数据,`LspTool::execute` 通过 `manager.get_handle(name)`
/// 拿 `Arc<LspClientInner>` 调 method。
pub struct LspServerHandle {
    pub server_name: String,
    pub client: Arc<LspClientInner>,
    /// 从 `ServerCapabilities` 推出来的 method 列表(给 TUI status_bar / CLI 展示用)。
    pub supported_methods: Vec<String>,
    /// 从 `file_patterns` 聚合的 language_id 列表。
    pub language_ids: Vec<String>,
    /// 启动用配置,reload diff 用。
    pub startup_config: LspServerConfig,
}

/// 生命周期事件,推到 `reflect-exec` 后转 `EventMsg::LspServerStarted/Failed`。
#[derive(Debug, Clone)]
pub enum LspLifecycleEvent {
    Started {
        server: String,
        methods: Vec<String>,
        language_ids: Vec<String>,
    },
    Failed {
        server: String,
        error: String,
        /// Phase A 永远为 `false`(LSP 不自动重连,配置错就让用户修)。
        will_retry: bool,
    },
    Stopped {
        server: String,
    },
}

/// 集中管理器。
///
/// 单例由 `bootstrap_lsp` 构造,`LspTool` 持有 `Arc<LspConnectionManager>`。
pub struct LspConnectionManager {
    handles: RwLock<HashMap<String, LspServerHandle>>,
    event_tx: mpsc::Sender<LspLifecycleEvent>,
    /// Phase B:被动缓存各 server 推送的 publishDiagnostics。
    diagnostics: Arc<DiagnosticRegistry>,
}

impl LspConnectionManager {
    /// 构造 manager(不启动任何 server)。
    pub fn new(event_tx: mpsc::Sender<LspLifecycleEvent>) -> Self {
        Self {
            handles: RwLock::new(HashMap::new()),
            event_tx,
            diagnostics: Arc::new(DiagnosticRegistry::new()),
        }
    }

    /// 诊断注册表(跨 server 共享)。
    pub fn diagnostics(&self) -> Arc<DiagnosticRegistry> {
        self.diagnostics.clone()
    }

    /// 当前在跑的 server 名。
    pub async fn server_names(&self) -> Vec<String> {
        self.handles.read().await.keys().cloned().collect()
    }

    /// 拿一个 server 的 client(`LspTool::execute` 用)。
    pub async fn get_client(&self, name: &str) -> Option<Arc<LspClientInner>> {
        self.handles
            .read()
            .await
            .get(name)
            .map(|h| h.client.clone())
    }

    /// 拿所有 handle(`reflect lsp list` CLI 用)。
    pub async fn all_handles(&self) -> Vec<LspServerHandle> {
        self.handles
            .read()
            .await
            .values()
            .map(|h| LspServerHandle {
                server_name: h.server_name.clone(),
                client: h.client.clone(),
                supported_methods: h.supported_methods.clone(),
                language_ids: h.language_ids.clone(),
                startup_config: h.startup_config.clone(),
            })
            .collect()
    }

    /// 启动一个 LSP server,跑完 initialize 握手。
    ///
    /// 错误路径返回 `LspError::Spawn`(子进程 spawn 失败)或
    /// `LspError::Initialize`(握手失败)。任一阶段都推
    /// `LspLifecycleEvent::Failed { will_retry: false }`。
    pub async fn start_server(&self, cfg: LspServerConfig) -> Result<LspServerHandle, LspError> {
        let name = cfg.name.clone();
        let root_uri = cfg.root_uri.as_ref();
        let init_options = cfg.initialization_options.as_ref();
        // spawn peer + initialize 握手。
        let (peer, capabilities, server_info) = match LspPeer::spawn_with_caps(
            &cfg.command,
            &cfg.args,
            &cfg.env,
            root_uri,
            init_options,
            Some(self.diagnostics.clone()),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(LspLifecycleEvent::Failed {
                        server: name.clone(),
                        error: format!("{e}"),
                        will_retry: false,
                    })
                    .await;
                return Err(e);
            }
        };
        let inner = Arc::new(LspClientInner::new(
            peer,
            name.clone(),
            server_info,
            capabilities,
            cfg.timeout,
        ));
        let supported_methods: Vec<String> = inner
            .supported_methods()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let language_ids: Vec<String> = cfg
            .patterns
            .iter()
            .map(|p| p.language_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let handle = LspServerHandle {
            server_name: name.clone(),
            client: inner,
            supported_methods: supported_methods.clone(),
            language_ids: language_ids.clone(),
            startup_config: cfg,
        };
        // 插 map。
        {
            let mut handles = self.handles.write().await;
            handles.insert(
                name.clone(),
                LspServerHandle {
                    server_name: handle.server_name.clone(),
                    client: handle.client.clone(),
                    supported_methods: handle.supported_methods.clone(),
                    language_ids: handle.language_ids.clone(),
                    startup_config: handle.startup_config.clone(),
                },
            );
        }
        // 推 Started 事件。
        let _ = self
            .event_tx
            .send(LspLifecycleEvent::Started {
                server: name,
                methods: supported_methods,
                language_ids,
            })
            .await;
        Ok(handle)
    }

    /// 关掉一个 server。返回被注销的 entry 名字(给 caller 反注册用)。
    pub async fn stop_server(&self, name: &str) -> Option<String> {
        let entry = {
            let mut handles = self.handles.write().await;
            handles.remove(name)?
        };
        // 1. close_all docs(best-effort,尽力而为)
        entry.client.close_all().await;
        // 2. cancel peer → 子进程 drop → kill_on_drop 杀进程
        entry.client.cancel.cancel();
        // 3. event(事件)
        let _ = self
            .event_tx
            .send(LspLifecycleEvent::Stopped {
                server: name.to_string(),
            })
            .await;
        Some(name.to_string())
    }

    /// 优雅 shutdown 所有 server。`reflect-exec` 在主进程退出前调,
    /// 让 stdio 子进程收到 cancel 而不是被 `kill_on_drop` 强杀。
    pub async fn shutdown(&self) {
        let names: Vec<String> = self.handles.read().await.keys().cloned().collect();
        for name in names {
            let _ = self.stop_server(&name).await;
        }
    }

    /// reload diff 算法。Phase A 留 stub,Phase B 接入 `handle_reload`。
    pub async fn reload(&self, new_configs: Vec<LspServerConfig>) -> Result<ReloadDiff, LspError> {
        let new_names: HashSet<String> = new_configs.iter().map(|c| c.name.clone()).collect();
        let old_names: HashSet<String> = self.handles.read().await.keys().cloned().collect();
        let added: Vec<String> = new_names.difference(&old_names).cloned().collect();
        let removed: Vec<String> = old_names.difference(&new_names).cloned().collect();
        Ok(ReloadDiff {
            added,
            removed,
            changed: vec![],
        })
    }
}

/// `reload` diff 结果。Phase A 仅 `added` / `removed` 字段有值。
#[derive(Debug, Clone, Default)]
pub struct ReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn make_cfg(name: &str) -> LspServerConfig {
        use crate::config::CompiledFilePattern;
        use std::collections::HashMap;
        LspServerConfig {
            name: name.to_string(),
            command: "nonexistent-binary".to_string(),
            args: vec![],
            env: HashMap::new(),
            patterns: vec![CompiledFilePattern::compile("**/*.rs", "rust").unwrap()],
            root_uri: None,
            initialization_options: None,
            timeout: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn start_server_with_bad_binary_emits_failed_event() {
        let (tx, mut rx) = mpsc::channel::<LspLifecycleEvent>(8);
        let mgr = LspConnectionManager::new(tx);
        let cfg = make_cfg("rust");
        let r = mgr.start_server(cfg).await;
        assert!(r.is_err());
        // 失败事件被推。
        let evt = rx.try_recv().expect("failed event");
        match evt {
            LspLifecycleEvent::Failed {
                server, will_retry, ..
            } => {
                assert_eq!(server, "rust");
                assert!(!will_retry);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_names_initially_empty() {
        let (tx, _rx) = mpsc::channel(8);
        let mgr = LspConnectionManager::new(tx);
        assert!(mgr.server_names().await.is_empty());
    }

    #[tokio::test]
    async fn reload_diff_reports_added_removed() {
        let (tx, _rx) = mpsc::channel(8);
        let mgr = LspConnectionManager::new(tx);
        let diff = mgr
            .reload(vec![make_cfg("rust"), make_cfg("go")])
            .await
            .unwrap();
        let mut added = diff.added.clone();
        added.sort();
        assert_eq!(added, vec!["go".to_string(), "rust".to_string()]);
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }
}
