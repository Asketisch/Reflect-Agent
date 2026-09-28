//! MCP Prompts 工具 —— `ListMcpPrompts` / `GetMcpPrompt`。
//!
//! v1.6 接线:与 Resources(`resources.rs`)对齐的第三类 MCP 原语。
//! `ListMcpPrompts` 逐个 server 调 `Peer::list_all_prompts()` 拉回
//! prompt 目录(名称 + 描述 + 参数);`GetMcpPrompt` 调
//! `Peer::get_prompt()` 按 server + name + arguments 渲染出消息序列,
//! 以 markdown 文本返回给 agent 直接消费。未连接任何 server 时返回
//! 空清单(而非错误)。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput};
use rmcp::model::{GetPromptRequestParams, PromptMessageContent, PromptMessageRole};

use crate::manager::McpConnectionManager;

/// 列出所有已连接 MCP server 暴露的 prompts(名称 + 描述 + 参数签名)。
///
/// 逐个 server 调 `list_all_prompts()`,失败按 server 聚合为告警。
pub struct ListMcpPromptsTool {
    manager: Arc<McpConnectionManager>,
}

impl ListMcpPromptsTool {
    pub fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ListMcpPromptsTool {
    fn name(&self) -> &str {
        "ListMcpPrompts"
    }

    fn description(&self) -> &str {
        "List MCP prompts exposed by all connected MCP servers (prompts/list). \
         Returns each prompt's server, name, description and argument signatures. \
         Use GetMcpPrompt to render a prompt into messages."
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
            match peer.list_all_prompts().await {
                Ok(prompts) => {
                    for p in prompts {
                        total += 1;
                        let mut line = format!("{}: {}", server, p.name);
                        if let Some(desc) = &p.description {
                            line.push_str(&format!(" — {desc}"));
                        }
                        if let Some(args) = &p.arguments {
                            let sig = args
                                .iter()
                                .map(|a| {
                                    if a.required.unwrap_or(false) {
                                        format!("{}(必填)", a.name)
                                    } else {
                                        a.name.clone()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            if !sig.is_empty() {
                                line.push_str(&format!(" [args: {sig}]"));
                            }
                        }
                        lines.push(line);
                    }
                }
                Err(e) => errors.push(format!("{server}: {e}")),
            }
        }

        let mut body = if total == 0 {
            format!("No MCP prompts reported by {server_count} connected server(s).\n")
        } else {
            let mut s = format!("MCP prompts ({total}):\n");
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
                "prompts": total,
                "errors": errors.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

/// 渲染指定 MCP prompt 为消息序列。
///
/// 参数:`server`(目标 server 名)、`name`(prompt 名)、
/// `arguments`(可选,k-v 字符串表)。消息按 role 标注渲染成 markdown。
pub struct GetMcpPromptTool {
    manager: Arc<McpConnectionManager>,
}

impl GetMcpPromptTool {
    pub fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for GetMcpPromptTool {
    fn name(&self) -> &str {
        "GetMcpPrompt"
    }

    fn description(&self) -> &str {
        "Render an MCP prompt by name from a named server (prompts/get). \
         Pass `server` (MCP server name), `name` (prompt name from ListMcpPrompts) \
         and optional `arguments` (object of string values for the prompt's \
         declared args). Returns the rendered message list as text."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "MCP server name (as shown by ListMcpPrompts)"
                },
                "name": {
                    "type": "string",
                    "description": "Prompt name to render"
                },
                "arguments": {
                    "type": "object",
                    "description": "Optional string-valued arguments for the prompt",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["server", "name"]
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
                    message: "GetMcpPrompt: missing 'server'".into(),
                })?;
        let name =
            args.get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "GetMcpPrompt: missing 'name'".into(),
                })?;
        // 可选 arguments:只接受字符串值(与 MCP 规范的 string args 对齐),
        // 非 string 值转成 to_string 后仍以字符串传递。
        let arguments: Option<serde_json::Map<String, serde_json::Value>> = args
            .get("arguments")
            .and_then(|v| v.as_object())
            .map(|obj| {
                let mut map = serde_json::Map::new();
                for (k, v) in obj {
                    let s = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    map.insert(k.clone(), serde_json::Value::String(s));
                }
                map
            });

        let peers = self.manager.server_peers().await;
        let peer = peers
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, p)| Arc::clone(p))
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "GetMcpPrompt: server '{server}' not connected (have: {})",
                    peers
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        // GetPromptRequestParams::new 仅接受 name;arguments 需手动装填
        // (rmcp 的构造器不带 args 参数版本)。
        let mut params = GetPromptRequestParams::new(name.to_string());
        params.arguments = arguments;
        let result = peer
            .get_prompt(params)
            .await
            .map_err(|e| ToolError::Execution(format!("GetMcpPrompt: rmcp error: {e}")))?;

        // 渲染消息序列:image / 嵌入资源以占位标注(与 Resources 的
        // blob 处理一致),文本内容原样返回。
        let mut parts: Vec<String> = Vec::new();
        for (i, msg) in result.messages.iter().enumerate() {
            let role = match msg.role {
                PromptMessageRole::User => "user",
                PromptMessageRole::Assistant => "assistant",
            };
            let content = match &msg.content {
                PromptMessageContent::Text { text } => text.clone(),
                PromptMessageContent::Image { image } => {
                    format!("[image data, {} bytes base64, omitted]", image.data.len())
                }
                PromptMessageContent::Resource { .. } => {
                    "[embedded resource, use ReadMcpResource to fetch]".to_string()
                }
                PromptMessageContent::ResourceLink { link } => {
                    format!("[resource link: {}]", link.uri)
                }
            };
            parts.push(format!("## message {} [{}]\n\n{}", i + 1, role, content));
        }
        let mut body = if parts.is_empty() {
            format!("prompt '{name}' rendered to 0 messages")
        } else {
            parts.join("\n\n")
        };
        if let Some(desc) = &result.description {
            body = format!("{desc}\n\n{body}");
        }

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "server": server,
                "name": name,
                "messages": result.messages.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_tools::Tool;

    /// 无 server 连接时,ListMcpPrompts 返回空清单(而非错误)。
    #[tokio::test]
    async fn list_mcp_prompts_empty_when_no_servers() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = ListMcpPromptsTool::new(mgr);
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .expect("list ok");
        assert!(!out.is_error);
        assert_eq!(out.metadata["servers"], 0);
        assert_eq!(out.metadata["prompts"], 0);
    }

    /// 缺 server / name 参数时,GetMcpPrompt 返回 InvalidArgs。
    #[tokio::test]
    async fn get_mcp_prompt_requires_server_and_name() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = GetMcpPromptTool::new(mgr);

        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "x"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs { .. })));

        let err = tool
            .execute(ToolContext::default(), serde_json::json!({"server": "s"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs { .. })));
    }

    /// server 不存在时,GetMcpPrompt 返回 Execution 错误并提示已连接清单。
    #[tokio::test]
    async fn get_mcp_prompt_unknown_server_errors() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::McpLifecycleEvent>(4);
        let mgr = Arc::new(McpConnectionManager::new(tx));
        let tool = GetMcpPromptTool::new(mgr);
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"server": "ghost", "name": "review"}),
            )
            .await
            .expect_err("unknown server");
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("not connected")),
            other => panic!("expected Execution, got {other:?}"),
        }
    }
}
