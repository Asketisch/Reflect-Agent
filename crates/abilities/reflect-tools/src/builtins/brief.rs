//! `brief` —— BriefTool(P2 `brief-tool`)。
//!
//! 整理附件/上下文为简短 briefing,供 LLM 快速消费。

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use reflect_protocol::ContentBlock;

pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn name(&self) -> &str {
        "brief"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.brief")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Brief title" },
                "content": { "type": "string", "description": "Raw text to summarize" },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths (stub: echoed only)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Max output length",
                    "default": 800
                }
            },
            "required": ["title"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let title =
            args.get("title")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "brief: missing 'title'".into(),
                })?;
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(800) as usize;
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let body = if content.is_empty() && !paths.is_empty() {
            format!("Attachments: {}", paths.join(", "))
        } else {
            content.to_string()
        };

        let truncated = if body.chars().count() > max_chars {
            body.chars().take(max_chars).collect::<String>() + "…"
        } else {
            body
        };

        let briefing = format!("## {title}\n\n{truncated}");
        Ok(ToolOutput {
            content: vec![ContentBlock::text(briefing.clone())],
            is_error: false,
            metadata: serde_json::json!({
                "title": title,
                "chars": truncated.chars().count(),
                "paths": paths,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn brief_truncates_long_content() {
        let tool = BriefTool;
        let long = "x".repeat(1000);
        let out = tool
            .execute(
                ToolContext::default(),
                json!({"title": "T", "content": long, "max_chars": 100}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!(),
        };
        assert!(text.contains('…'));
    }
}
