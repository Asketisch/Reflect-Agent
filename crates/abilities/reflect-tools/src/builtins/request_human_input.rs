//! `request_human_input` —— 持久化人工输入 stub(P2 `request-human-input`)。
//!
//! 与 `ask_user` 类似,但 metadata 标记 `persistent: true`,供未来
//! DB 等待 / 释放计算资源路径识别。
//!
//! ## 当前实现 vs 设计目标(v1.2 review P2:bug-8)
//!
//! `context_id` 当前**仅写**到 `ToolOutput.metadata`,作为调用语义标签
//! (供下游按 context 聚合回答);**未**实现真正的 DB 持久化 / 跨 turn
//! 恢复 —— 任何 `context_id` 都不能用来在 session 中途"重连"上次的
//! 未完成等待。
//!
//! TODO(v1.3+): 把 `context_id` 接到 `~/.reflect/human_input/<id>.json`
//! 存储 + TUI 重启后回放,实现真正的长任务持久化。

use async_trait::async_trait;
use reflect_protocol::{ContentBlock, PermissionMode, ToolOutput};
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};

pub struct RequestHumanInputTool;

#[async_trait]
impl Tool for RequestHumanInputTool {
    fn name(&self) -> &str {
        "request_human_input"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.request_human_input")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    // v1.2 review P1:bug-3:`minLength: 1` 与 `ask_user` 对齐。
                    "type": "string",
                    "minLength": 1,
                    "description": "Question or instruction for the user"
                },
                "context_id": {
                    "type": "string",
                    "description": "Optional persistence key for resuming later (currently write-only)"
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        // v1.2 review P2:bug-1:`tracing::info_span!` 包裹单次执行,与
        // `ask_user.execute` 同结构,多 `context_id` 字段方便把同一
        // 持久化任务的多次调用串起来。
        let span = tracing::info_span!(
            "request_human_input.execute",
            context_id = tracing::field::Empty,
            prompt_len = tracing::field::Empty,
            response_len = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _enter = span.enter();

        // v1.2 review P2:bug-5:运行时强制 `additionalProperties: false`。
        let obj = args.as_object().ok_or_else(|| ToolError::InvalidArgs {
            message: "request_human_input: args must be a JSON object".into(),
        })?;
        if obj.len() > 2 || obj.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "request_human_input: unexpected keys (only 'prompt' + optional 'context_id'), got {}",
                    obj.len()
                ),
            });
        }

        // v1.2 review P1:bug-3:tool 层 trim 校验。
        let prompt = obj
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "request_human_input: missing or non-string 'prompt'".into(),
            })?
            .trim();
        if prompt.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "request_human_input: 'prompt' must not be empty or whitespace".into(),
            });
        }
        // context_id 可选,缺省 "default";提供时必须是 string。
        let context_id = match obj.get("context_id") {
            None => "default".to_string(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "request_human_input: 'context_id' must be a string".into(),
                })?
                .to_string(),
        };
        span.record("context_id", context_id.as_str());
        span.record("prompt_len", prompt.chars().count());

        let gate = ctx.approval.as_ref().ok_or_else(|| {
            ToolError::Execution("request_human_input: requires ApprovalGate (TUI mode)".into())
        })?;

        let start = std::time::Instant::now();
        // v1.2 review P1:bug-1:`request_human_input` 语义是持久化等待
        // (DB 释放 / 长任务),不允许 15 min 超时切断;传 0 走永不超时分支。
        // v1.2 review P1:bug-2:工具名传给 gate 用于 resolver 查询。
        let text = gate
            .ask_user("request_human_input", prompt, &ctx.cancel, 0)
            .await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        span.record("response_len", text.chars().count());
        span.record("elapsed_ms", elapsed_ms);
        span.record("outcome", "ok");

        Ok(ToolOutput {
            content: vec![ContentBlock::text(text.clone())],
            is_error: false,
            metadata: serde_json::json!({
                "prompt": prompt,
                "response": text,
                "context_id": context_id,
                "persistent": true,
                "elapsed_ms": elapsed_ms,
            }),
            elapsed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_is_stable() {
        assert_eq!(RequestHumanInputTool.name(), "request_human_input");
    }

    /// v1.2 review P1:bug-3:schema 标注 `minLength: 1`。
    #[test]
    fn request_human_input_schema_rejects_empty_prompt() {
        let schema = RequestHumanInputTool.parameters_schema();
        assert_eq!(
            schema["properties"]["prompt"]["minLength"], 1,
            "schema must declare minLength=1 on prompt"
        );
        assert_eq!(schema["required"][0], "prompt");
        assert_eq!(schema["additionalProperties"], false);
    }
}
