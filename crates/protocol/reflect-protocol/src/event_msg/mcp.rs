//! v0.3 MCP server 生命周期 + 工具调用载荷。

use serde::{Deserialize, Serialize};

/// v0.3 M6: 一个 MCP server 握手 + `list_tools` 成功。
///
/// `tool_count` 由 manager 启动路径填,给 TUI 在 status_bar 显示
/// `│ mcp: N servers / M tools` 用。`transport` 是镜像 enum,
/// 因为 `reflect-protocol` 不能依赖 `reflect-config` (反向依赖风险),
/// `reflect-mcp::config` 提供 `From<McpTransportMirror> for McpTransport`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerStartedEvent {
    pub server: String,
    pub tool_count: usize,
    /// 批次十八:工具全名列表(供 TUI `/mcp` overlay 展示工具清单)。
    #[serde(default)]
    pub tool_names: Vec<String>,
    pub transport: McpTransportMirror,
}

/// v0.3 M6: 一个 MCP server 启动失败(spawn / initialize / list_tools 任一阶段)。
///
/// `will_retry` 为 `true` 表示 HTTP 重连循环还会继续尝试;
/// stdio 路径永远为 `false`(不重连,配置错就让用户修)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerFailedEvent {
    pub server: String,
    pub error: String,
    pub will_retry: bool,
}

/// v0.3 M6: 单次 MCP tool call 完成。
///
/// `server.tool` 拼接字符串直接给 TUI 在 status_bar 高亮调用链。
/// `call_id` 与 `EventMsg::ToolCallEnd.call_id` 一致,便于前端配对渲染。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolInvokedEvent {
    pub server: String,
    pub tool: String,
    pub call_id: String,
}

/// v0.3 M6: `reflect_config::McpTransport` 的镜像,避免 protocol → config 反向依赖。
///
/// 用 `lowercase` 序列化(`"stdio"` / `"http"`),与 config schema 端一致。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportMirror {
    Stdio,
    Http,
    /// Legacy MCP HTTP+SSE transport。
    Sse,
}

impl McpTransportMirror {
    /// 小写字符串,与 serde 序列化一致(`"stdio"` / `"http"` / `"sse"`)。
    /// TUI overlay / Pill 渲染用此避免每处都写 match。
    pub fn as_str(self) -> &'static str {
        match self {
            McpTransportMirror::Stdio => "stdio",
            McpTransportMirror::Http => "http",
            McpTransportMirror::Sse => "sse",
        }
    }
}

impl std::fmt::Display for McpTransportMirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
