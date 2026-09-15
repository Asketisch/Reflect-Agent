//! `request_human_input` —— 持久化人工输入 stub(P2 `request-human-input`)。
//!
//! 与 `ask_user` 类似,但 metadata 标记 `persistent: true`,供未来
//! DB 等待 / 释放计算资源路径识别。
//!
//! ## v1.5 E2:持久化闭环(原 TODO 落地)
//!
//! 接入 `ToolContext.human_input`(`HumanInputStore`)后,等待期间写
//! 挂起文件 `~/.reflect/human_input/<context_id>.json`;外部进程
//! (TUI 重启后 / 任意客户端)写 `<context_id>.answer.json` 即可在
//! 轮询窗口内应答 —— **跨进程 / 跨重启闭环**。TUI modal 回执与文件
//! 应答双通道竞争,先到先得;完成(或取消)后文件一并清除。
//!
//! 存储缺席(旧调用方)时维持原行为:仅 TUI gate 等待。

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
                    "description": "Optional persistence key; a pending request file is written and an external process may answer via <id>.answer.json"
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
        // v1.5 E2:持久化闭环。store 在位 → 写挂起文件;等待 = TUI gate
        // 回执 与 外部应答文件轮询 双通道竞争。store 缺席 → 原 TUI-only 行为。
        let store = ctx.human_input.clone();
        if let Some(st) = &store {
            st.write_pending(&context_id, prompt);
            // 快路径:外部已预写答案(跨重启恢复)→ 无需 gate。
            if let Some(answer) = st.poll_answer(&context_id) {
                span.record("response_len", answer.chars().count());
                span.record("elapsed_ms", start.elapsed().as_millis() as u64);
                span.record("outcome", "ok");
                return Ok(Self::output(prompt, &context_id, answer, start));
            }
        }

        let gate_required = gate;
        let wait_tui = async {
            // v1.2 review P1:bug-1:`request_human_input` 语义是持久化等待
            // (DB 释放 / 长任务),不允许 15 min 超时切断;传 0 走永不超时分支。
            // v1.2 review P1:bug-2:工具名传给 gate 用于 resolver 查询。
            gate_required
                .ask_user("request_human_input", prompt, &ctx.cancel, 0)
                .await
        };
        let wait_file = async {
            // 500ms 轮询外部应答文件;取消令牌贯通(会话关闭即退出)。
            loop {
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                if let Some(st) = &store {
                    if let Some(answer) = st.poll_answer(&context_id) {
                        return Ok(answer);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        };
        let text = tokio::select! {
            r = wait_tui => r?,
            r = wait_file => r?,
        };
        if let Some(st) = &store {
            st.clear(&context_id);
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;
        span.record("response_len", text.chars().count());
        span.record("elapsed_ms", elapsed_ms);
        span.record("outcome", "ok");

        Ok(Self::output(prompt, &context_id, text, start))
    }
}

impl RequestHumanInputTool {
    /// 统一的输出构造(prompt / response / context_id 元数据)。
    fn output(
        prompt: &str,
        context_id: &str,
        text: String,
        start: std::time::Instant,
    ) -> ToolOutput {
        let elapsed_ms = start.elapsed().as_millis() as u64;
        ToolOutput {
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
        }
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

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::approval::ApprovalGate;
    use crate::human_input::HumanInputStore;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// 外部应答文件闭环:挂起文件写出 → 外部写 answer → 工具立即返回
    /// 答案并清理两份文件(gate 永不被阻塞)。
    #[tokio::test]
    async fn file_answer_short_circuits_without_modal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HumanInputStore::new(dir.path()));
        let (tx, _rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(8);
        let gate = Arc::new(ApprovalGate::new(tx, "t1"));

        // 预写答案(模拟外部进程先应答)。
        std::fs::write(
            dir.path().join("api-key.answer.json"),
            r#"{"answer":"sk-from-file"}"#,
        )
        .unwrap();

        let mut tctx = ToolContext::for_workspace(".");
        tctx.approval = Some(gate);
        tctx.human_input = Some(store.clone());
        tctx.cancel = CancellationToken::new();

        let out = RequestHumanInputTool
            .execute(
                tctx,
                serde_json::json!({"prompt": "give me key", "context_id": "api-key"}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(text, "sk-from-file");
        assert_eq!(out.metadata["context_id"], "api-key");
        assert!(out.metadata["persistent"].as_bool().unwrap());
        // 两份文件都已清理。
        assert!(store.list_pending().is_empty());
        assert!(!dir.path().join("api-key.answer.json").exists());
    }

    /// 挂起文件生命周期:等待期间存在,文件应答完成后清除。
    #[tokio::test]
    async fn pending_file_written_then_cleared_on_file_answer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HumanInputStore::new(dir.path()));
        let (tx, _rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(8);
        let gate = Arc::new(ApprovalGate::new(tx, "t2"));

        let mut tctx = ToolContext::for_workspace(".");
        tctx.approval = Some(gate.clone());
        tctx.human_input = Some(store.clone());
        let tctx = {
            let mut t = tctx;
            t.cancel = CancellationToken::new();
            t
        };

        let task = tokio::spawn(async move {
            RequestHumanInputTool
                .execute(
                    tctx,
                    serde_json::json!({"prompt": "confirm deploy", "context_id": "deploy"}),
                )
                .await
        });

        // 等挂起文件出现(工具已进入等待)。
        for _ in 0..40 {
            if !store.list_pending().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "deploy");
        assert_eq!(pending[0].1.prompt, "confirm deploy");

        // 外部进程应答。
        std::fs::write(
            dir.path().join("deploy.answer.json"),
            r#"{"answer":"go ahead"}"#,
        )
        .unwrap();

        let out = timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("execute in time")
            .unwrap()
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(text, "go ahead");
        assert!(store.list_pending().is_empty(), "完成后挂起文件清除");
    }

    use tokio::time::timeout;
}
