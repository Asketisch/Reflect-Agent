//! 单个 LSP server 的运行时内部状态。
//!
//! `LspClientInner` 由 `LspConnectionManager` 持有,`LspTool::execute`
//! 拿到对应 server 的 handle 后通过 `manager.handles[].client` 拿
//! `Arc<LspClientInner>`,然后 `client.peer.request::<...>(...)` 调
//! LSP method。
//!
//! ## 字段
//!
//! - `peer`:stdio JSON-RPC peer(在 `transport.rs` 实现)。
//! - `server_name`:用户配置里 `[lsp_servers.<name>]` 的 name,日志用。
//! - `server_info`:LSP `initialize` 响应里的 `serverInfo` 字段。
//! - `capabilities`:LSP `initialize` 响应里的 `ServerCapabilities`,
//!   `LspTool` 调 method 前可以查 `capabilities.definition_provider` 等
//!   决定是否走该 path。
//! - `cancel`:shutdown 触发器,`LspConnectionManager::stop_server` 调它
//!   → 内部 reader/writer task 退出 → 子进程 drop → `kill_on_drop` 杀进程。
//! - `docs`:已 `didOpen` 的文档 map,`document::ensure_open` 维护。
//! - `timeout`:单次 request 超时(`tokio::time::timeout` 上限)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lsp_types::{ServerCapabilities, ServerInfo, Uri};
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use crate::document::DocumentState;
use crate::transport::LspPeer;

/// 单个 LSP server 已建立连接后的运行时内部状态。
///
/// `Arc<LspClientInner>` 由 `LspConnectionManager` 持有,`LspTool::execute`
/// 通过 `manager.get_handle(name).client` 拿到后,`client.peer.request::<...>(...)`
/// 调 LSP method,`client.ensure_open(...)` 维护文档同步。
pub struct LspClientInner {
    /// JSON-RPC peer(读 / 写 task + inflight oneshot 表)。
    pub peer: Arc<LspPeer>,
    /// 用户配置里 `[lsp_servers.<name>]` 的 name,日志与 `ServerInfo` 区别。
    pub server_name: String,
    /// LSP `initialize` 响应里的 `ServerInfo`(name / version)。
    pub server_info: Option<ServerInfo>,
    /// LSP `initialize` 响应里的 `ServerCapabilities`。
    pub capabilities: ServerCapabilities,
    /// shutdown 触发器。`stop_server` 调它 → 内部 task 退出 → child drop。
    pub cancel: CancellationToken,
    /// 已 `didOpen` 文档表(URI → 状态)。
    pub docs: RwLock<HashMap<Uri, DocumentState>>,
    /// 单次 request 超时。
    pub timeout: Duration,
}

impl LspClientInner {
    /// 构造一个新 inner。`LspConnectionManager::start_server` 走
    /// `LspPeer::spawn(...)` 拿 peer 后调用。
    pub fn new(
        peer: Arc<LspPeer>,
        server_name: String,
        server_info: Option<ServerInfo>,
        capabilities: ServerCapabilities,
        timeout: Duration,
    ) -> Self {
        Self {
            peer,
            server_name,
            server_info,
            capabilities,
            cancel: CancellationToken::new(),
            docs: RwLock::new(HashMap::new()),
            timeout,
        }
    }

    /// 报告 server 实际可用的 LSP method 集合(给 `LspLifecycleEvent::Started` 字段用)。
    /// 从 `ServerCapabilities` 推断 —— 6 个 Phase B1 只读 method 的 provider flag。
    pub fn supported_methods(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        let caps = &self.capabilities;
        if caps.definition_provider.is_some() {
            out.push("textDocument/definition");
        }
        if caps.references_provider.is_some() {
            out.push("textDocument/references");
        }
        if caps.hover_provider.is_some() {
            out.push("textDocument/hover");
        }
        if caps.document_symbol_provider.is_some() {
            out.push("textDocument/documentSymbol");
        }
        if caps.completion_provider.is_some() {
            out.push("textDocument/completion");
        }
        if caps.signature_help_provider.is_some() {
            out.push("textDocument/signatureHelp");
        }
        out
    }
}
