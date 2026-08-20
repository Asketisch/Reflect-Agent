//! `tool_search` — 按关键词搜索 `ToolRegistry` 中已注册工具的 spec。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{ToolRegistry, ToolSource};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// 持有 registry 引用,在 `list_specs` 结果上做关键词匹配。
pub struct ToolSearchTool {
    registry: Arc<ToolRegistry>,
}

impl ToolSearchTool {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    /// 对 name / description 做大小写不敏感子串匹配,按 name 字典序返回。
    fn search(&self, query: &str, limit: usize) -> Vec<(String, String, ToolSource)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(String, String, ToolSource)> = self
            .registry
            .list_with_source()
            .into_iter()
            .filter_map(|(name, source)| {
                let tool = self.registry.get(&name)?;
                let desc = tool.description();
                if name.to_lowercase().contains(&q) || desc.to_lowercase().contains(&q) {
                    Some((name, desc.to_string(), source))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits.truncate(limit);
        hits
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.tool_search")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword to match against tool name or description"
                },
                "limit": {
                    "type": "number",
                    "description": "Max results (default 20, max 100)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let query =
            args.get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'query'".into(),
                })?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        let hits = self.search(query, limit);
        if hits.is_empty() {
            return Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(format!(
                    "No tools matched query '{query}'"
                ))],
                is_error: false,
                metadata: serde_json::json!({"query": query, "count": 0}),
                elapsed_ms: 0,
            });
        }

        let mut lines = Vec::with_capacity(hits.len());
        for (name, desc, source) in &hits {
            let src = match source {
                ToolSource::Builtin => "builtin",
                ToolSource::Runtime => "runtime",
                ToolSource::Plugin => "plugin",
                // v1.3:`ToolSource::Mcp` 新增;搜索结果中标注 MCP 工具
                // 的来源,便于区分。
                ToolSource::Mcp => "mcp",
                // v1.3 SDK:客户端注册的远程自定义工具。
                ToolSource::Remote => "remote",
            };
            lines.push(format!("{name} [{src}]: {desc}"));
        }
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(lines.join("\n"))],
            is_error: false,
            metadata: serde_json::json!({
                "query": query,
                "count": hits.len(),
                "tools": hits.iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::ReadTool;

    #[tokio::test]
    async fn search_finds_by_name_and_description() {
        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(ReadTool));
        reg.register(Arc::new(ToolSearchTool::new(reg.clone())));

        let tool = ToolSearchTool::new(reg.clone());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"query": "read file"}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("read"));
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let reg = Arc::new(ToolRegistry::default());
        reg.register(Arc::new(ReadTool));
        let tool = ToolSearchTool::new(reg);
        assert!(tool.search("   ", 10).is_empty());
    }
}
