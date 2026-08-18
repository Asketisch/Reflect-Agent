//! `custom_tool` —— 注册一个用户自定义 `Tool` 并在 prompt 中使用。
//!
//! 实现了一个 `EchoTool`(在存储字符串上做空操作包装)——
//! 真实场景下,把方法体替换为 HTTP 请求、数据库查询等即可。
//!
//! 运行:
//! ```bash
//! OPENAI_API_KEY=sk-... cargo run -p reflect --example custom_tool
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use reflect::{Reflect, Submission, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::Value;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the user's text back as the tool output. Useful as a placeholder when wiring up a custom tool registry."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "text to echo back"}
            },
            "required": ["text"]
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let text =
            args.get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'text'".into(),
                })?;
        Ok(ToolOutput {
            content: vec![reflect::ContentBlock::text(text)],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 构造 model registry + tool registry,挂上自定义 tool。
    let agent = Reflect::builder("openai/gpt-4o").build()?;
    let tools: Arc<ToolRegistry> = agent.thread().tools().clone();
    tools.register(Arc::new(EchoTool));

    // 2. 提交一个让 LLM 调用 `echo` 的 prompt。
    let prompt = "请用 text 'hello from custom_tool' 调用 echo 工具。";
    let mut stream = agent.submit(Submission::user_input(prompt)).await;

    while let Some(event) = stream.next().await {
        match event.msg {
            reflect::EventMsg::ToolCallEnd(end) => {
                println!(
                    "[tool] {} ok={} output={:?}",
                    end.call_id, !end.is_error, end.output
                );
            }
            reflect::EventMsg::AgentMessageDelta(d) => print!("{}", d.delta),
            reflect::EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    Ok(())
}
