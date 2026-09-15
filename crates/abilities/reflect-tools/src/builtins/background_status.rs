//! `background_status` —— v1.5 R2:查询后台任务(id / 状态 / 输出)。

use async_trait::async_trait;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub struct BackgroundStatusTool;

#[async_trait]
impl Tool for BackgroundStatusTool {
    fn name(&self) -> &str {
        "background_status"
    }

    fn description(&self) -> &str {
        "List background tasks (id / status / output). Completed results are also injected at turn boundaries."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let Some(spawner) = ctx.background.as_ref() else {
            return Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(
                    "本线程未接入后台任务(无后台任务生成器)",
                )],
                is_error: false,
                metadata: serde_json::json!({"supported": false}),
                elapsed_ms: 0,
            });
        };
        let tasks = spawner.snapshot();
        let text = if tasks.is_empty() {
            "(no background tasks)".to_string()
        } else {
            tasks
                .iter()
                .map(|t| {
                    let head = format!("[{}] {}", t.id, t.status);
                    match &t.result {
                        Some(r) => format!("{head}\n{}", {
                            let mut r = r.clone();
                            if r.len() > 2000 {
                                r.truncate(2000);
                                r.push_str("\n... [truncated]");
                            }
                            r
                        }),
                        None => head,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(text)],
            is_error: false,
            metadata: serde_json::json!({"count": tasks.len()}),
            elapsed_ms: 0,
        })
    }
}
