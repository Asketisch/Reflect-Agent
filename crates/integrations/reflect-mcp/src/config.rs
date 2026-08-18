//! MCP server 强类型配置。
//!
//! 在 `reflect_config::McpServerEntry` 之上加一层 "rmcp-ready" 表示,
//! `McpConnectionManager::start_server` 直接消费本层类型。
//!
//! 完整实现见 `Step 2` / `Step 4`。本文件先放最小桩以让
//! `cargo build -p reflect-mcp` 通过。

use std::collections::HashMap;
use std::time::Duration;

use reflect_config::McpServerConfigShape;

/// MCP transport 类型,对应 TOML `type = "stdio" | "http"`。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpTransport {
    #[default]
    Stdio,
    /// Streamable HTTP,别名 `streamable-http`。
    Http,
    /// Legacy MCP HTTP+SSE(GET events + POST endpoint)。
    Sse,
}

impl From<reflect_config::McpTransport> for McpTransport {
    fn from(t: reflect_config::McpTransport) -> Self {
        match t {
            reflect_config::McpTransport::Stdio => Self::Stdio,
            reflect_config::McpTransport::Http => Self::Http,
            reflect_config::McpTransport::Sse => Self::Sse,
        }
    }
}

/// 编译期就绪的 MCP server 配置(`reflect_config::McpServerConfigShape` 的薄包装)。
///
/// `reflect-config` 不直接出 `McpServerConfig` 是为了避免它在
/// `reflect-mcp` 之下依赖过深;这里在 reflect-mcp 侧做形态转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    /// stdio 模式:子进程可执行文件路径。
    pub command: Option<String>,
    /// stdio 模式:子进程参数。
    pub args: Vec<String>,
    /// stdio 模式:除 `HOME`/`PATH` 之外注入到子进程的 env。
    pub env: HashMap<String, String>,
    /// http 模式:server URL。
    pub url: Option<String>,
    /// http 模式:自定义 HTTP header(`Authorization` 自动剥离到 `auth_header`)。
    pub headers: HashMap<String, String>,
    /// 单次 tool call 超时,默认 30s。
    pub timeout: Duration,
}

impl McpServerConfig {
    /// 单次 tool call 超时。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl From<McpServerConfigShape> for McpServerConfig {
    fn from(s: McpServerConfigShape) -> Self {
        Self {
            name: s.name,
            transport: McpTransport::from(s.transport),
            command: s.command,
            args: s.args,
            env: s.env,
            url: s.url,
            headers: s.headers,
            timeout: s.timeout,
        }
    }
}
