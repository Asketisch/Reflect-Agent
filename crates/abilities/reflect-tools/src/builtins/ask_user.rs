//! `ask_user` —— v1.1.0 P1 #15:LLM 向用户发起自由文本询问。
//!
//! 与 `ask_user_question`(结构化多选题)区分:只接受一条 `prompt`,
//! TUI 弹单行 input modal,用户输入通过 `Op::AskUserInputResponse` 回执。

use async_trait::async_trait;
use reflect_protocol::{ContentBlock, PermissionMode, ToolOutput};
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.ask_user")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    // v1.2 review P1:bug-3:`minLength: 1` 让 LLM 在 schema
                    // 层就知道空 prompt 不会被接受,减少一两次 round-trip。
                    "type": "string",
                    "minLength": 1,
                    "description": "The question or instruction shown to the user"
                },
                // v1.2 P0:敏感输入掩码(API key / password / token)。
                "secret": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, mask the user's input as dots (for secrets). Default false."
                },
                "placeholder": {
                    "type": "string",
                    "description": "Optional placeholder shown when the input field is empty."
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
        // v1.2 review P2:bug-1:`tracing::info_span!` 包裹单次执行,字段
        // `prompt_len` 入参常量直接 record,`response_len` / `elapsed_ms`
        // / `outcome` 在结束时回填,便于事后追查 agent 究竟问什么、
        // 用户答什么、最终状态(成功 / 取消 / 超时 / 拒答)。
        let span = tracing::info_span!(
            "ask_user.execute",
            prompt_len = tracing::field::Empty,
            response_len = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _enter = span.enter();

        // v1.2 review P2:bug-5:运行时强制 `additionalProperties: false`。
        // v1.2 P0:放宽为允许 prompt + 可选 secret + 可选 placeholder。
        let obj = args.as_object().ok_or_else(|| ToolError::InvalidArgs {
            message: "ask_user: args must be a JSON object".into(),
        })?;
        // 校验只允许这三个键。
        for key in obj.keys() {
            if !matches!(key.as_str(), "prompt" | "secret" | "placeholder") {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "ask_user: unexpected key '{key}' (allowed: prompt, secret, placeholder)"
                    ),
                });
            }
        }

        // v1.2 review P1:bug-3:在 tool 层加 trim 校验,gate 层 `is_empty`
        // 检查保留作为最后兜底。LLM 发 `prompt: "   "` 在工具入口就被拒。
        let prompt = obj
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "ask_user: missing or non-string 'prompt'".into(),
            })?
            .trim();
        if prompt.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "ask_user: 'prompt' must not be empty or whitespace".into(),
            });
        }
        span.record("prompt_len", prompt.chars().count());

        // v1.2 P0:解析可选的 secret / placeholder。
        let secret = obj.get("secret").and_then(|v| v.as_bool()).unwrap_or(false);
        let placeholder = obj
            .get("placeholder")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let gate = ctx.approval.as_ref().ok_or_else(|| {
            ToolError::Execution(
                "ask_user: no ApprovalGate available (headless mode not supported)".into(),
            )
        })?;

        let start = std::time::Instant::now();
        // v1.2 review P1:bug-1:与 `AskUserSection::default_timeout_secs` 默认
        // 900 秒对齐(15 分钟),0 表示永不超时。`request_human_input` 路径
        // 显式传 0 保留持久化语义。
        // v1.2 review P1:bug-2:工具名传给 gate 用于 resolver 查询("ask_user")。
        // v1.2 P0:透传 secret / placeholder。
        let text = gate
            .ask_user_opts(
                "ask_user",
                prompt,
                &ctx.cancel,
                900,
                secret,
                placeholder.as_deref(),
            )
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
    fn ask_user_tool_metadata() {
        let t = AskUserTool;
        assert_eq!(t.name(), "ask_user");
        assert!(!t.is_concurrency_safe());
        assert_eq!(t.required_permission(), PermissionMode::Auto);
    }

    /// v1.2 review P1:bug-3:schema 标注 `minLength: 1`。
    /// v1.2 P0:`secret` / `placeholder` 为可选字段,prompt 仍唯一必填。
    #[test]
    fn ask_user_schema_rejects_empty_prompt() {
        let schema = AskUserTool.parameters_schema();
        assert_eq!(
            schema["properties"]["prompt"]["minLength"], 1,
            "schema must declare minLength=1 on prompt"
        );
        assert_eq!(schema["required"][0], "prompt");
        assert_eq!(schema["required"].as_array().unwrap().len(), 1);
        assert_eq!(schema["properties"]["secret"]["type"], "boolean");
        assert_eq!(schema["properties"]["secret"]["default"], false);
        assert_eq!(schema["properties"]["placeholder"]["type"], "string");
        assert_eq!(schema["additionalProperties"], false);
    }
}
