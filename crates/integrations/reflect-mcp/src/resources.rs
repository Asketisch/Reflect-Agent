//! MCP Resources 工具 —— `ListMcpResources` / `ReadMcpResource`。
//!
//! v1.2.0 接线:此前仅返回 stub 字符串。现在真正调用 rmcp peer 的
//! `list_all_resources()` / `read_resource()`,把每个连接 server 暴露的
//! resources 拉回给 agent。未连接任何 server 时返回空(而非错误)。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput};
use rmcp::model::ResourceContents;

use crate::manager::McpConnectionManager;

/// 列出所有已连接 MCP server 暴露的资源。
///
/// 逐个 server 调 `Peer::list_all_resources()`,失败按 server 聚合为告警
/// (不中断其余 server)。返回每行 `server: name <uri>` 的文本清单。
pub struct ListMcpResourcesTool {
    manager: Arc<McpConnectionManager>,
}

impl ListMcpResourcesTool {
    pub fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        "ListMcpResources"
    }

    fn description(&self) -> &str {
        "List MCP resources exposed by all connected MCP servers (resources/list). \
         Returns each resource's server, name and URI."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let peers = self.manager.server_peers().await;
        let server_count = peers.len();
        let mut lines: Vec<String> = Vec::new();
        let mut total = 0usize;
        let mut errors: Vec<String> = Vec::new();

        for (server, peer) in peers {
            match peer.list_all_resources().await {
                Ok(resources) => {
                    for r in resources {
                        let raw = &r.raw;
                        total += 1;
                        lines.push(format!("{}: {} <{}>", server, raw.name, raw.uri));
                    }
                }
                Err(e) => errors.push(format!("{server}: {e}")),
            }
        }

        let mut body = if total == 0 {
            format!("No MCP resources reported by {server_count} connected server(s).\n")
        } else {
            let mut s = format!("MCP resources ({total}):\n");
            s.push_str(&lines.join("\n"));
            s.push('\n');
            s
        };
        if !errors.is_empty() {
            body.push_str(&format!("\nlisting errors:\n{}\n", errors.join("\n")));
        }

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "servers": server_count,
                "resources": total,
                "errors": errors.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

/// 读取指定 MCP 资源。
///
/// 参数:`server`(目标 server 名)、`uri`(资源 URI)。逐个匹配的 server
/// 调 `Peer::read_resource()`;命中即返回内容(text / blob),找不到则报错。
pub struct ReadMcpResourceTool {
    manager: Arc<McpConnectionManager>,
}

impl ReadMcpResourceTool {
    pub fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        "ReadMcpResource"
    }

    fn description(&self) -> &str {
        "Read an MCP resource by URI from a named server (resources/read). \
         Pass `server` (MCP server name) and `uri`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "MCP server name (as shown by ListMcpResources)"
                },
                "uri": { "type": "string", "description": "Resource URI to read" }
            },
            "required": ["server", "uri"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let server =
            args.get("server")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "ReadMcpResource: missing 'server'".into(),
                })?;
        let uri =
            args.get("uri")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "ReadMcpResource: missing 'uri'".into(),
                })?;

        let peers = self.manager.server_peers().await;
        let peer = peers
            .iter()
            .find(|(name, _)| name == server)
            .map(|(_, p)| Arc::clone(p))
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "ReadMcpResource: server '{server}' not connected (have: {})",
                    peers
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        let params = rmcp::model::ReadResourceRequestParams::new(uri);
        let result = peer
            .read_resource(params)
            .await
            .map_err(|e| ToolError::Execution(format!("ReadMcpResource: rmcp error: {e}")))?;

        // 把 ResourceContents(text/blob)渲染成文本给 LLM 消费。
        let mut lines: Vec<String> = Vec::new();
        let mut text_parts: Vec<String> = Vec::new();
        for c in &result.contents {
            match c {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    text_parts.push(text.clone());
                    lines.push(format!("text <{uri}>"));
                }
                ResourceContents::BlobResourceContents { uri, blob, .. } => {
                    lines.push(format!(
                        "blob <{uri}> ({} bytes base64, omitted from text view)",
                        blob.len()
                    ));
                }
            }
        }
        let body = if !text_parts.is_empty() {
            text_parts.join("\n\n---\n\n")
        } else if !lines.is_empty() {
            lines.join("\n")
        } else {
            format!("resource <{uri}>: empty contents")
        };

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "server": server,
                "uri": uri,
                "contents": result.contents.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_tools::Tool;

    /// 无 server 连接时,ListMcpResources 返回空清单(而非错误)。
    #[tokio::test]
    async fn list_mcp_resources_empty_when_no_servers() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = ListMcpResourcesTool::new(mgr);
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .expect("list ok");
        assert!(!out.is_error);
        assert_eq!(out.metadata["servers"], 0);
        assert_eq!(out.metadata["resources"], 0);
    }

    /// 缺 server / uri 参数时,ReadMcpResource 返回 InvalidArgs。
    #[tokio::test]
    async fn read_mcp_resource_requires_server_and_uri() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = ReadMcpResourceTool::new(mgr);

        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"uri": "x"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs { .. })));

        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"server": "s"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs { .. })));
    }

    /// server 不存在时,ReadMcpResource 返回 Execution 错误并提示已连接清单。
    #[tokio::test]
    async fn read_mcp_resource_unknown_server_errors() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = ReadMcpResourceTool::new(mgr);
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"server": "ghost", "uri": "file:///x"}),
            )
            .await
            .expect_err("unknown server");
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("not connected")),
            other => panic!("expected Execution, got {other:?}"),
        }
    }
}
