//! `reflect-mcp` — MCP (Model Context Protocol) client integration (v0.3)。
//!
//! 每个 MCP server 暴露 N 个 tool,通过 `Tool` trait 包装后注册到
//! `ToolRegistry::Runtime`。LLM 层 (`reflect-llm`) 无需改动:OpenAI /
//! Anthropic 的 tool schema 转换直接吃 JSON Schema 2020-12,与 MCP
//! `inputSchema` 同型。
//!
//! ## 范围 (v0.3)
//!
//! - transport: stdio + streamable-http
//! - 原语: Tools
//! - 协议版本: `2025-06-18`
//!
//! ## 能力清单(v1.6 更正)
//!
//! - 已实现:Tools、Resources(`ListMcpResources` / `ReadMcpResource`,
//!   v1.2 起真接线 —— 此前注释误标为未实现)、Prompts
//!   (`ListMcpPrompts` / `GetMcpPrompt`,v1.6 接线)。
//! - 未实现(留后续):Sampling / Elicitation / OAuth。
//!
//! ## 关键约束
//!
//! - Tool 名强制 `mcp__<server>__<tool>` 前缀,与 7 个 Reflect
//!   内置 tool 不冲突。
//! - 配置走 `[mcp_servers.<name>]` TOML 表风格(对齐 `.mcp.json`)。
//! - env 注入用 `env_clear()` 隔离,只传 `HOME`/`PATH` + 用户显式 env,
//!   避免泄漏 `OPENAI_API_KEY` 等密钥到 MCP 子进程。

#![allow(clippy::module_name_repetitions)] // 多个模块都暴露 *Config/*Error 等命名

pub mod config;
pub mod content;
pub mod manager;
pub mod prompts;
pub mod protocol_version;
pub mod registry;
pub mod resources;
pub mod tool;
pub mod transport;

// 公开 API re-export
pub use config::{McpServerConfig, McpTransport};
pub use content::convert_mcp_content;
pub use manager::{
    McpConnectionManager, McpLifecycleEvent, McpServerHandle, McpServerStatus, list_plugin_servers,
    scoped_plugin_name,
};
pub use prompts::{GetMcpPromptTool, ListMcpPromptsTool};
pub use protocol_version::PROTOCOL_VERSION;
pub use registry::{McpRegistryEntry, curated_catalog, find_entry, install_hint};
pub use resources::{ListMcpResourcesTool, ReadMcpResourceTool};
pub use tool::{McpToolAdapter, McpToolDescriptor};

/// MCP 集成错误类型。
///
/// 所有 `Tool::execute` 失败路径都映射回 `reflect_protocol::ToolError`,
/// 此处 `McpError` 仅用于 `McpConnectionManager::start_server` 与
/// `reload` 等生命周期操作。
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// 配置校验失败(server 缺 command/url 等)。
    #[error("mcp config invalid: {0}")]
    ConfigInvalid(String),

    /// 启动 stdio 子进程失败。
    #[error("mcp spawn failed: {0}")]
    Spawn(#[from] std::io::Error),

    /// 握手 / initialize 失败。
    #[error("mcp initialize failed: {0}")]
    Initialize(String),

    /// 单次 tool call 失败。
    #[error("mcp call failed: {0}")]
    Call(String),

    /// 单次 tool call 超过 `timeout_ms`。
    #[error("mcp call timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// 用户 (ctrl-c) 或 manager shutdown 触发 cancel。
    #[error("mcp call cancelled")]
    Cancelled,

    /// 优雅 shutdown 超时(>10s),需要 SIGKILL。
    #[error("mcp shutdown timeout")]
    ShutdownTimeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_exposed() {
        assert_eq!(PROTOCOL_VERSION, "2025-06-18");
    }

    #[test]
    fn error_display_includes_kind() {
        let e = McpError::Timeout { elapsed_ms: 250 };
        assert!(e.to_string().contains("250"));
    }
}
