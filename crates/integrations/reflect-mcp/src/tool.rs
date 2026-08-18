//! `McpToolAdapter` — 把单个 MCP tool 包成 `reflect_tools::Tool`。
//!
//! ## 关键约束
//!
//! - 工具全名强制 `mcp__<server>__<tool>` 前缀,与 7 个 Reflect
//!   内置工具不冲突。
//! - 双 cancel:`ToolContext::cancel`(ctrl-c)+ `McpClientInner::cancel`
//!   (manager shutdown)同时监听,tokio::select! 任一触发立即返
//!   `ToolError::Cancelled`。
//! - 单次 tool call 超时由 `McpServerConfig.timeout` 控制,默认 30s。
//! - 错误路径:rmcp 错误 → `ToolError::Execution`;
//!   超时 → `ToolError::Timeout { elapsed_ms }`;
//!   cancel → `ToolError::Cancelled`。
//! - `required_permission` 默认 `Prompt` —— MCP server 不可信,Reflect
//!   M6 `ApprovalGate` 在 `ToolExecutionQueue` 路径上做每次确认。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolError, ToolOutput};
use reflect_tools::{Tool, ToolContext};
use rmcp::model::CallToolRequestParams;

use crate::content::convert_mcp_content;
use crate::manager::McpClientInner;

/// MCP server 暴露的单个 tool 描述符,由 `list_all_tools` 缓存。
#[derive(Debug, Clone)]
pub struct McpToolDescriptor {
    /// Reflect 命名空间全名,例如 `mcp__filesystem__read_file`。
    pub full_name: String,
    /// MCP 原始名(去掉前缀),用于 `CallToolRequestParams::new`。
    pub original_name: String,
    /// LLM 可见的 description。
    pub description: String,
    /// MCP `inputSchema` (JSON Schema 2020-12),透传到 `Tool::parameters_schema`。
    pub input_schema: serde_json::Value,
    /// 从 `ToolAnnotations.read_only_hint` 推导的并发安全标记。
    pub is_concurrency_safe: bool,
}

/// `Tool` trait 适配器。
pub struct McpToolAdapter {
    pub full_name: String,
    pub original_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub server_name: String,
    pub required_permission: PermissionMode,
    pub is_concurrency_safe: bool,
    /// 真实 server 启动时由 `from_descriptor` 填 `Some`;测试用 `None`,
    /// execute 路径会立即返 `ToolError::Execution`("client not initialized")。
    pub client: Option<Arc<McpClientInner>>,
    pub timeout: Duration,
}

impl McpToolAdapter {
    /// 由 `McpToolDescriptor` + `McpClientInner` 构造 adapter。
    pub fn from_descriptor(
        inner: Arc<McpClientInner>,
        desc: &McpToolDescriptor,
        server_name: &str,
        timeout: Duration,
    ) -> Self {
        Self {
            full_name: desc.full_name.clone(),
            original_name: desc.original_name.clone(),
            description: desc.description.clone(),
            input_schema: desc.input_schema.clone(),
            server_name: server_name.to_string(),
            // MCP tool 默认 `Prompt` —— server 提供方不可信,Reflect
            // 通过 `ApprovalGate` (M6) 让用户确认每次调用。
            required_permission: PermissionMode::Prompt,
            is_concurrency_safe: desc.is_concurrency_safe,
            client: Some(inner),
            timeout,
        }
    }

    /// 仅用 descriptor 构造(不接 `client`),供测试 / 调试场景使用;
    /// 实际注册到 `ToolRegistry` 的实例必须用 `from_descriptor`。
    #[cfg(test)]
    pub fn for_tests(desc: &McpToolDescriptor, server_name: &str, timeout: Duration) -> Self {
        Self {
            full_name: desc.full_name.clone(),
            original_name: desc.original_name.clone(),
            description: desc.description.clone(),
            input_schema: desc.input_schema.clone(),
            server_name: server_name.to_string(),
            required_permission: PermissionMode::Prompt,
            is_concurrency_safe: desc.is_concurrency_safe,
            client: None,
            timeout,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.is_concurrency_safe
    }
    fn required_permission(&self) -> PermissionMode {
        self.required_permission
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        // 参数校验先于 client 检查 —— 错误信息更直观。
        let args_map = match args {
            serde_json::Value::Object(m) => m,
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "mcp tool arguments must be a JSON object, got {}",
                        type_name_of(&other)
                    ),
                });
            }
        };
        let client = self.client.as_ref().ok_or_else(|| {
            ToolError::Execution("mcp tool adapter has no client (test-only?)".to_string())
        })?;
        let started = Instant::now();
        let timeout = self.timeout;
        let cancel_token = ctx.cancel.clone();
        let inner_cancel = client.cancel.clone();

        // 双 cancel:外部 ctrl-c + manager shutdown 触发任一就返 Cancelled。
        // `biased` 保证先检查 cancel(低开销),再调实际 MCP call。
        let call_fut = async {
            tokio::time::timeout(
                timeout,
                client.peer.call_tool(
                    CallToolRequestParams::new(self.original_name.clone()).with_arguments(args_map),
                ),
            )
            .await
        };
        let result = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                return Err(ToolError::Cancelled);
            }
            _ = inner_cancel.cancelled() => {
                return Err(ToolError::Cancelled);
            }
            r = call_fut => r,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let result = result.map_err(|_| ToolError::Timeout {
            elapsed_ms: timeout.as_millis() as u64,
        })?;
        let result = result.map_err(|e| {
            ToolError::Execution(format!("mcp call_tool({}): {e}", self.original_name))
        })?;
        let content = convert_mcp_content(&result.content);
        let is_error = result.is_error.unwrap_or(false);
        let metadata = serde_json::json!({
            "server": self.server_name,
            "original_name": self.original_name,
            "mcp_is_error": is_error,
            "elapsed_ms": elapsed_ms,
        });
        Ok(ToolOutput {
            content,
            is_error,
            metadata,
            elapsed_ms,
        })
    }
}

fn type_name_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_tools::Tool;

    fn make_descriptor(readonly: bool) -> McpToolDescriptor {
        McpToolDescriptor {
            full_name: "mcp__mock__echo".to_string(),
            original_name: "echo".to_string(),
            description: "echo text".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            }),
            is_concurrency_safe: readonly,
        }
    }

    // 真正的 execute() 测试需要 rmcp server,留 Step 10 集成测试。
    // 这里只验证 metadata / schema / name 等非网络字段。

    #[test]
    fn from_descriptor_copies_full_name() {
        let desc = make_descriptor(true);
        let adapter = McpToolAdapter::for_tests(&desc, "mock", Duration::from_secs(30));
        assert_eq!(adapter.name(), "mcp__mock__echo");
        assert_eq!(adapter.description(), "echo text");
        assert!(adapter.is_concurrency_safe());
        assert_eq!(
            adapter.parameters_schema()["properties"]["text"]["type"],
            "string"
        );
    }

    #[test]
    fn default_required_permission_is_prompt() {
        let desc = make_descriptor(false);
        let adapter = McpToolAdapter::for_tests(&desc, "mock", Duration::from_secs(30));
        assert_eq!(adapter.required_permission(), PermissionMode::Prompt);
    }

    #[test]
    fn readonly_hint_propagates_to_concurrency_safe() {
        let desc_ro = make_descriptor(true);
        let adapter_ro = McpToolAdapter::for_tests(&desc_ro, "mock", Duration::from_secs(30));
        assert!(adapter_ro.is_concurrency_safe());

        let desc_rw = make_descriptor(false);
        let adapter_rw = McpToolAdapter::for_tests(&desc_rw, "mock", Duration::from_secs(30));
        assert!(!adapter_rw.is_concurrency_safe());
    }

    #[test]
    fn full_name_uses_double_underscore_separator() {
        let desc = McpToolDescriptor {
            full_name: "mcp__filesystem__read_file".to_string(),
            original_name: "read_file".to_string(),
            description: "r".to_string(),
            input_schema: serde_json::json!({}),
            is_concurrency_safe: false,
        };
        assert!(desc.full_name.starts_with("mcp__"));
        assert!(desc.full_name.contains("__filesystem__"));
        assert!(desc.full_name.ends_with("__read_file"));
    }

    #[tokio::test]
    async fn execute_without_client_returns_execution_error() {
        let desc = make_descriptor(false);
        let adapter = McpToolAdapter::for_tests(&desc, "mock", Duration::from_secs(30));
        let ctx = ToolContext::default();
        let err = adapter
            .execute(ctx, serde_json::json!({"text": "hi"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("no client")),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_rejects_non_object_arguments() {
        let desc = make_descriptor(false);
        let adapter = McpToolAdapter::for_tests(&desc, "mock", Duration::from_secs(30));
        let ctx = ToolContext::default();
        let err = adapter
            .execute(ctx, serde_json::json!("not an object"))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { message } => {
                assert!(message.contains("JSON object"));
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }
}
