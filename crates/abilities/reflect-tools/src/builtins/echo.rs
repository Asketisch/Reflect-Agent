//! `echo` —— no-op 工具,原样返回输入。适合测试 / 演示。

use async_trait::async_trait;
use reflect_protocol::ToolOutput;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.echo")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Text to echo"}
            },
            "required": ["text"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'text'".into(),
            })?
            .to_string();
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(text)],
            is_error: false,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_returns_input_text() {
        let t = EchoTool;
        let ctx = ToolContext::for_workspace(".");
        let out = t
            .execute(ctx, serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.elapsed_ms == 0);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn echo_rejects_missing_text() {
        let t = EchoTool;
        let ctx = ToolContext::default();
        let err = t.execute(ctx, serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
