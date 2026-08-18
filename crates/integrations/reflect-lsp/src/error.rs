//! LSP 集成错误类型。
//!
//! 所有 `Tool::execute` 失败路径都映射回 `reflect_protocol::ToolError`,
//! 此处 `LspError` 仅用于 `LspConnectionManager::start_server` 与
//! `reload` 等生命周期操作。
//!
//! ## 错误分类
//!
//! - **配置 / 启动阶段**:`ConfigInvalid` / `Spawn` / `Initialize` ——
//!   `bootstrap_lsp` 路径上 warn + 跳过整 server。
//! - **单次 method 调用**:`Call` / `ServerError` / `Timeout` / `Cancelled` —
//!   `LspTool::execute` 路径上转 `ToolError::*`。
//! - **优雅 shutdown**:`ShutdownTimeout` —— 与 MCP 同语义,主进程退出前
//!   子进程未在 10s 内自愿退出会触发,需要 SIGKILL。

/// LSP 集成错误。镜像 `reflect_mcp::McpError` 形态。
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// 配置校验失败(server 缺 command、glob 编译失败等)。
    #[error("lsp config invalid: {0}")]
    ConfigInvalid(String),

    /// 启动 stdio 子进程失败。
    #[error("lsp spawn failed: {0}")]
    Spawn(#[from] std::io::Error),

    /// 握手 / initialize 失败。
    #[error("lsp initialize failed: {0}")]
    Initialize(String),

    /// 单次 method 调用失败(transport 层 / JSON-RPC 错误 / 协议错误)。
    #[error("lsp call failed: {0}")]
    Call(String),

    /// server 返回的 JSON-RPC 错误响应(对应 LSP `ResponseError`)。
    #[error("lsp server error: code={code}, message={message}")]
    ServerError { code: i32, message: String },

    /// 单次 request 超过 `LspServerConfig.timeout`。
    #[error("lsp call timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// 用户 (ctrl-c) 或 manager shutdown 触发 cancel。
    #[error("lsp call cancelled")]
    Cancelled,

    /// 优雅 shutdown 超时(>10s),需要 SIGKILL。
    #[error("lsp shutdown timeout")]
    ShutdownTimeout,

    /// 其他兜底错误(globset 编译失败等,本不该发生)。
    #[error("lsp glob error: {0}")]
    Glob(String),

    /// Path / URL 转换错误(workspace 路径无法转 LSP URI)。
    #[error("lsp path error: {0}")]
    Path(String),
}

/// 把 std `io::Error` 走 `From` 派生即可,这里留 placeholder 让 `Spawn`
/// 走 `#[from]` 自动转换。
impl From<globset::Error> for LspError {
    fn from(e: globset::Error) -> Self {
        LspError::Glob(e.to_string())
    }
}

impl From<url::ParseError> for LspError {
    fn from(e: url::ParseError) -> Self {
        LspError::Path(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_kind() {
        let e = LspError::Timeout { elapsed_ms: 500 };
        assert!(e.to_string().contains("500"));
        let e = LspError::ServerError {
            code: -32601,
            message: "Method not found".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("-32601"));
        assert!(s.contains("Method not found"));
    }
}
