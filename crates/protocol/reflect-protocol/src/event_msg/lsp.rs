//! v0.5 LSP server 生命周期载荷。

use serde::{Deserialize, Serialize};

/// v0.5: 一个 LSP server 握手 + `initialize` 成功。
///
/// `methods` 字段是从 `ServerCapabilities` 推出来的 method 列表(当前
/// Phase A 是 `textDocument/definition` / `references` / `hover` 之一
/// 或多者),`language_ids` 是该 server 接管的 LSP languageId 集合
///(去重)。TUI status_bar 可显示 `│ lsp: N servers` + 每个 server
/// 拥有的 method 数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspServerStartedEvent {
    pub server: String,
    pub methods: Vec<String>,
    pub language_ids: Vec<String>,
}

/// v0.5: 一个 LSP server 启动失败(`spawn` / `initialize` 任一阶段)。
///
/// `will_retry` 永远为 `false`(LSP 不自动重连,配置错就让用户修)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspServerFailedEvent {
    pub server: String,
    pub error: String,
    pub will_retry: bool,
}
