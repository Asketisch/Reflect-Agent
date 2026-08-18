//! MCP 连接生命周期管理:start_server / reload diff / shutdown。
//!
//! 启动流程:
//! 1. `transport::build()` 构造 stdio / http transport 并跑 initialize 握手
//! 2. `Peer<RoleClient>::list_all_tools()` 缓存 tool 描述符
//! 3. 把 handle 插 `handles` map
//! 4. 推 `McpLifecycleEvent::Started`
//!
//! 重载流程(增量 diff):
//! - `added`:新配置里有但 handles 里没有 → 启动
//! - `removed`:handles 里有但新配置里没有 → 停掉 + 通知 caller 反注册 tool
//! - `changed`:handles 与新配置都有但字段值变了 → 停 + 启动
//!
//! Shutdown(stdio):排空 handles,对每个 entry 走 transport `cancel`。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::RoleClient;
use rmcp::model::Tool;
use rmcp::service::Peer;
use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::McpError;
use crate::config::{McpServerConfig, McpTransport};
use crate::tool::McpToolDescriptor;
use crate::transport;

/// 单个 MCP server 已建立连接后的运行时内部状态。
///
/// `Arc<McpClientInner>` 由 `McpToolAdapter` 持有,用于并发调用
/// `call_tool` / `list_all_tools`。
pub struct McpClientInner {
    /// rmcp client peer。
    pub peer: Arc<Peer<RoleClient>>,
    pub server_name: String,
    /// shutdown 时打断 in-flight call + 当外部 cancel 触发时 `tokio::select!`
    /// 优先返回 `Cancelled`。
    pub cancel: CancellationToken,
    /// status 变化广播(Started / Failed / Stopped),给 TUI / 监控用。
    pub status_tx: watch::Sender<McpServerStatus>,
}

/// MCP server 在 manager 中的 entry。
pub struct McpServerHandle {
    pub server_name: String,
    /// 启动后 list_all_tools 缓存(给 reload 时复用 + 给 caller 注册 adapter)。
    pub tools: Vec<McpToolDescriptor>,
    pub inner: Arc<McpClientInner>,
    /// HTTP 重连 task(stdio 不启动);留 Option 以便未来切换协议栈。
    pub task: Option<JoinHandle<()>>,
    pub transport_kind: McpTransport,
    /// 给 reload diff 算法用。
    pub startup_config: McpServerConfig,
}

/// MCP server 状态广播。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    Started,
    Failed { error: String },
    Stopped,
}

/// 生命周期事件,推到 `reflect-exec` 后转 `EventMsg::McpServerStarted/Failed`。
#[derive(Debug, Clone)]
pub enum McpLifecycleEvent {
    Started {
        server: String,
        tools: usize,
        /// 批次十八:工具全名列表(从 `tool_descriptors.full_name`),供 TUI
        /// `/mcp` overlay 展示工具清单(而非只看聚合 count)。
        tool_names: Vec<String>,
        transport: McpTransport,
    },
    Failed {
        server: String,
        error: String,
        will_retry: bool,
    },
    Stopped {
        server: String,
    },
}

/// 集中管理器。
pub struct McpConnectionManager {
    handles: RwLock<HashMap<String, McpServerHandle>>,
    event_tx: mpsc::Sender<McpLifecycleEvent>,
}

/// v1.0.0-rc2: 把 plugin 提供的 server name 加 plugin 命名空间。
///
/// 命名规则:`plugin:<plugin_id>:<original_name>`,scoped 命名空间。
/// `ToolRegistry` / `PluginManager::unload`
/// 都靠这个前缀区分 builtin / 用户配置 / plugin 三类 MCP server。
pub fn scoped_plugin_name(plugin_id: &str, original_name: &str) -> String {
    format!("plugin:{plugin_id}:{original_name}")
}

/// 给定 plugin 的所有 scoped server 名(`server_names` 过滤)。
/// `PluginManager::unload` 用此找出该 plugin 注册过的 server,逐一 stop。
pub fn list_plugin_servers(all_servers: &[String], plugin_id: &str) -> Vec<String> {
    let prefix = format!("plugin:{plugin_id}:");
    all_servers
        .iter()
        .filter(|n| n.starts_with(&prefix))
        .cloned()
        .collect()
}

impl McpConnectionManager {
    /// 构造 manager(不启动任何 server,留给 caller 调 `start_server`)。
    pub fn new(event_tx: mpsc::Sender<McpLifecycleEvent>) -> Self {
        Self {
            handles: RwLock::new(HashMap::new()),
            event_tx,
        }
    }

    /// 当前在跑的 server 名。
    pub async fn server_names(&self) -> Vec<String> {
        self.handles.read().await.keys().cloned().collect()
    }

    /// 当前在跑的 server 快照:`(server_name, peer)` 对。
    ///
    /// 给 `ListMcpResources` / `ReadMcpResource` 工具直接调用 rmcp peer 的
    /// `list_all_resources()` / `read_resource()`,避免每个工具各自读锁。
    pub async fn server_peers(&self) -> Vec<(String, Arc<Peer<RoleClient>>)> {
        self.handles
            .read()
            .await
            .values()
            .map(|h| (h.server_name.clone(), Arc::clone(&h.inner.peer)))
            .collect()
    }

    /// 启动一个 MCP server,完整跑完 initialize + list_all_tools。
    ///
    /// 错误路径返回 `McpError::Initialize` 或 `Spawn`,由 caller 决定
    /// 是否 warn + 跳过整 server。
    pub async fn start_server(&self, cfg: McpServerConfig) -> Result<McpServerHandle, McpError> {
        let name = cfg.name.clone();
        let transport_kind = cfg.transport;
        // 失败路径需要 emit Failed event,但 transport 类型决定 will_retry:
        // stdio 不重试,http 由 reconnect task 重试。
        let will_retry = matches!(
            cfg.transport,
            crate::config::McpTransport::Http | crate::config::McpTransport::Sse
        );
        let build_result = transport::build(
            cfg.transport,
            cfg.command.as_deref(),
            &cfg.args,
            &cfg.env,
            cfg.url.as_deref(),
            &cfg.headers,
        )
        .await;
        let handle = match build_result {
            Ok(h) => h,
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(McpLifecycleEvent::Failed {
                        server: name.clone(),
                        error: format!("{e}"),
                        will_retry,
                    })
                    .await;
                return Err(e);
            }
        };
        // list_all_tools 一次拉全(rmcp 内部自动分页)。
        let tools = match handle.peer.list_all_tools().await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("list_all_tools: {e}");
                let _ = self
                    .event_tx
                    .send(McpLifecycleEvent::Failed {
                        server: name.clone(),
                        error: msg.clone(),
                        will_retry,
                    })
                    .await;
                return Err(McpError::Initialize(msg));
            }
        };
        let tool_descriptors = tools
            .iter()
            .map(|t| tool_descriptor_from_rmcp(&name, t))
            .collect::<Vec<_>>();
        let (status_tx, _status_rx) = watch::channel(McpServerStatus::Started);
        let inner = Arc::new(McpClientInner {
            peer: Arc::clone(&handle.peer),
            server_name: name.clone(),
            cancel: handle.cancel.clone(),
            status_tx,
        });
        let server_handle = McpServerHandle {
            server_name: name.clone(),
            tools: tool_descriptors.clone(),
            inner,
            task: None,
            transport_kind,
            startup_config: cfg,
        };
        // 插 map。
        {
            let mut handles = self.handles.write().await;
            handles.insert(
                name.clone(),
                McpServerHandle {
                    server_name: server_handle.server_name.clone(),
                    tools: server_handle.tools.clone(),
                    inner: Arc::clone(&server_handle.inner),
                    task: None,
                    transport_kind: server_handle.transport_kind,
                    startup_config: server_handle.startup_config.clone(),
                },
            );
        }
        // 把 cancel token 也保存到 handle 上,shutdown 时调它。
        // (此处 inner.cancel 已经共享;无需额外存储。)
        let _ = handle; // drop 触发 RunningService DropGuard
        // 通知 caller。
        let _ = self
            .event_tx
            .send(McpLifecycleEvent::Started {
                server: name,
                tools: tool_descriptors.len(),
                tool_names: tool_descriptors
                    .iter()
                    .map(|d| d.full_name.clone())
                    .collect(),
                transport: transport_kind,
            })
            .await;
        Ok(server_handle)
    }

    /// v1.0.0-rc2: 启动一个 plugin 提供的 MCP server。
    ///
    /// 把 `original_name` 在内部改写为 `plugin:<plugin_id>:<original_name>`,
    /// 然后复用 `start_server` 的 transport + initialize + list_all_tools 流程。
    /// 反注册走 `stop_server(scoped_name)`。
    ///
    /// 设计要点:
    /// - **scoped_name 是 manager 内部唯一标识** —— 不会与 builtin MCP
    ///   server 名冲突,也不会与其他 plugin 的同名 server 冲突。
    /// - **server_handle 内的 tool full name 仍带 `mcp__` 前缀**(沿用
    ///   `tool_descriptor_from_rmcp`),但前缀后是 scoped_name —— 因此
    ///   `ToolRegistry` 通过 `mcp__plugin:foo:fs__read_file` 这种名字
    ///   区分 plugin tool。
    /// - **plugin unload** 时,`PluginManager` 调 `stop_server(scoped_name)`
    ///   拿到所有 tool 名,再 `tools.unregister(...)`。
    pub async fn start_server_with_namespace(
        &self,
        plugin_id: &str,
        original_name: &str,
        cfg_template: McpServerConfig,
    ) -> Result<McpServerHandle, McpError> {
        let scoped = scoped_plugin_name(plugin_id, original_name);
        let mut cfg = cfg_template;
        cfg.name = scoped;
        self.start_server(cfg).await
    }

    /// 关掉一个 server(从 handles 移除 + cancel)。用于 reload 的
    /// `removed` / `changed` 分支。返回被移除的 tool 全名,给 caller
    /// 反注册 `ToolRegistry`。
    pub async fn stop_server(&self, name: &str) -> Option<Vec<String>> {
        let mut handles = self.handles.write().await;
        let entry = handles.remove(name)?;
        // cancel in-flight calls + 触发 TokioChildProcess DropGuard 杀子进程。
        entry.inner.cancel.cancel();
        // 通知 caller (TUI status_bar / 监控)。
        let _ = self
            .event_tx
            .send(McpLifecycleEvent::Stopped {
                server: name.to_string(),
            })
            .await;
        Some(entry.tools.iter().map(|t| t.full_name.clone()).collect())
    }

    /// reload diff 算法。`new_configs` 已经按 name 排序。
    ///
    /// `on_remove` 回调每停一个 server 调一次,参数是 tool 全名列表,
    /// 由 caller 决定怎么从 `ToolRegistry` 反注册。
    ///
    /// 整体算法:
    /// 1. 算 removed / changed / added 三组
    /// 2. 对 removed + changed:先停(start_server 失败的话旧 handle 还在)
    /// 3. 对 changed:在新 process 启动成功后,旧 handle 才正式 remove
    /// 4. 对 added:start_server + register
    pub async fn reload<F, R>(
        &self,
        new_configs: Vec<McpServerConfig>,
        mut on_remove: F,
    ) -> Result<Vec<String>, McpError>
    where
        F: FnMut(&str, &[String]) -> R,
    {
        let new_names: HashSet<String> = new_configs.iter().map(|c| c.name.clone()).collect();
        let old_names: Vec<String> = self.handles.read().await.keys().cloned().collect();

        // removed: 旧有但新没有 → 停 + 反注册 tool
        let mut stopped_tools: Vec<String> = Vec::new();
        for name in &old_names {
            if !new_names.contains(name)
                && let Some(tool_names) = self.stop_server(name).await
            {
                on_remove(name, &tool_names);
                stopped_tools.extend(tool_names);
            }
        }

        // changed: 新旧都在,字段变了 → 停旧 + 启新
        let new_by_name: HashMap<String, McpServerConfig> = new_configs
            .iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();
        let old_snapshots: Vec<(String, McpServerConfig)> = {
            let handles = self.handles.read().await;
            handles
                .values()
                .map(|h| (h.server_name.clone(), h.startup_config.clone()))
                .collect()
        };
        for (name, old_cfg) in old_snapshots {
            if let Some(new_cfg) = new_by_name.get(&name)
                && !entry_unchanged(&old_cfg, new_cfg)
            {
                tracing::info!(server = %name, "MCP server config changed, restarting");
                // 先停旧
                if let Some(tool_names) = self.stop_server(&name).await {
                    on_remove(&name, &tool_names);
                    stopped_tools.extend(tool_names);
                }
                // 再启新
                let new_cfg = new_cfg.clone();
                match self.start_server(new_cfg).await {
                    Ok(handle) => {
                        // tool 注册由 caller (bootstrap_m6 风格) 处理;
                        // 我们只确保 server 启动完成。
                        let _ = handle;
                    }
                    Err(e) => {
                        tracing::warn!(server = %name, error = %e,
                            "MCP server failed to start after config change");
                    }
                }
            }
        }

        // added: 新有但旧没有 → 启动
        let mut added_tools: Vec<String> = Vec::new();
        let existing: HashSet<String> = self.handles.read().await.keys().cloned().collect();
        for cfg in new_configs {
            if !existing.contains(&cfg.name) {
                match self.start_server(cfg.clone()).await {
                    Ok(handle) => {
                        for t in &handle.tools {
                            added_tools.push(t.full_name.clone());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(server = %cfg.name, error = %e,
                            "MCP server failed to start on reload add");
                    }
                }
            }
        }
        // 返回:被反注册的 tool 全名 + 被新注册的 tool 全名,给 caller 决定
        // 是否需要重新调度。
        stopped_tools.extend(added_tools);
        Ok(stopped_tools)
    }

    /// 优雅 shutdown 所有 server。等同于对每个 entry 调 `stop_server`。
    /// 由 `reflect-exec` 在主进程退出前调,让 stdio 子进程收到 SIGTERM
    /// 而不是被 `kill_on_drop` 强杀。
    pub async fn shutdown(&self) {
        let names: Vec<String> = self.handles.read().await.keys().cloned().collect();
        for name in names {
            let _ = self.stop_server(&name).await;
        }
    }
}

/// 比对 `old_cfg` 与 `new_cfg` 的启动关键字段,判断是否需要 restart。
///
/// `timeout` 变化不触发 restart —— 它在下一次 tool call 生效即可,
/// 没必要重启已经握手成功的 server。
fn entry_unchanged(old: &McpServerConfig, new: &McpServerConfig) -> bool {
    old.transport == new.transport
        && old.command == new.command
        && old.args == new.args
        && old.env == new.env
        && old.url == new.url
        && old.headers == new.headers
}

/// 从 rmcp `Tool` 构造 `McpToolDescriptor`。
fn tool_descriptor_from_rmcp(server_name: &str, tool: &Tool) -> McpToolDescriptor {
    let full_name = format!("mcp__{server_name}__{}", tool.name);
    let is_concurrency_safe = tool
        .annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false);
    // MCP inputSchema 是 JSON Schema 2020-12,Reflect 直接吃。
    // `tool.input_schema: Arc<JsonObject>`,JsonObject = serde_json::Map<String, Value>。
    // `*tool.input_schema` deref Arc → Map,`.clone()` 走 `Map` 的 Clone impl。
    let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
    McpToolDescriptor {
        full_name,
        original_name: tool.name.to_string(),
        description: tool
            .description
            .as_deref()
            .unwrap_or("(no description)")
            .to_string(),
        input_schema,
        is_concurrency_safe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn make_cfg(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(command.to_string()),
            args: vec!["--port".to_string(), "9000".to_string()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn entry_unchanged_detects_command_change() {
        let a = make_cfg("fs", "echo");
        let mut b = a.clone();
        b.command = Some("cat".to_string());
        assert!(!entry_unchanged(&a, &b));
    }

    #[test]
    fn entry_unchanged_detects_arg_change() {
        let a = make_cfg("fs", "echo");
        let mut b = a.clone();
        b.args = vec!["--help".to_string()];
        assert!(!entry_unchanged(&a, &b));
    }

    #[test]
    fn entry_unchanged_detects_env_change() {
        let a = make_cfg("fs", "echo");
        let mut b = a.clone();
        b.env.insert("FOO".into(), "bar".into());
        assert!(!entry_unchanged(&a, &b));
    }

    #[test]
    fn entry_unchanged_true_for_identical_cfgs() {
        let a = make_cfg("fs", "echo");
        let b = a.clone();
        assert!(entry_unchanged(&a, &b));
    }

    #[test]
    fn entry_unchanged_ignores_timeout_change() {
        // timeout 变化不需要 restart。
        let a = make_cfg("fs", "echo");
        let mut b = a.clone();
        b.timeout = Duration::from_secs(60);
        assert!(entry_unchanged(&a, &b));
    }

    #[test]
    fn entry_unchanged_detects_transport_change() {
        let a = make_cfg("fs", "echo");
        let mut b = a.clone();
        b.transport = McpTransport::Http;
        b.url = Some("https://x".into());
        assert!(!entry_unchanged(&a, &b));
    }

    // ── v1.0.0-rc2: plugin 命名空间 ─────────────────────────────────────

    #[test]
    fn scoped_plugin_name_uses_plugin_prefix() {
        assert_eq!(scoped_plugin_name("foo", "bar"), "plugin:foo:bar");
        assert_eq!(
            scoped_plugin_name("code-formatter", "lint"),
            "plugin:code-formatter:lint"
        );
    }

    #[test]
    fn list_plugin_servers_filters_by_prefix() {
        let servers = vec![
            "fs".to_string(),
            "github".to_string(),
            "plugin:foo:fs".to_string(),
            "plugin:foo:github".to_string(),
            "plugin:bar:fs".to_string(),
        ];
        let foo_servers = list_plugin_servers(&servers, "foo");
        assert_eq!(
            foo_servers,
            vec!["plugin:foo:fs".to_string(), "plugin:foo:github".to_string()]
        );
    }

    #[test]
    fn list_plugin_servers_unknown_plugin_returns_empty() {
        let servers = vec!["plugin:foo:fs".to_string()];
        assert!(list_plugin_servers(&servers, "bar").is_empty());
    }
}
