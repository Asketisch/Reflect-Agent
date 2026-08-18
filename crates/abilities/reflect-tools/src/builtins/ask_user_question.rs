//! `ask_user_question` —— v1.1.0 P1 #14:LLM 主动向用户发起结构化询问。
//!
//! ## 行为
//!
//! 1. 解析 args → `Vec<Question>`(1-4 道,每道 2-4 选项,`header` ≤ 12 字符)。
//! 2. 调 `ApprovalGate::ask_question(questions, &ctx.cancel)` 同步等待用户回执。
//! 3. 收到 `AskUserAnswer` 后,序列化为 JSON 文本塞进 `ToolOutput.content`。
//!
//! ## 取消 / 错误
//!
//! - `ctx.cancel` 触发 → `ToolError::Cancelled`(LLM 看到 tool failed,
//!   自行决定重试或换路径)。
//! - event channel 关闭(headless 无 TUI)→ `ToolError::Execution`。
//! - 参数越界 → `ToolError::InvalidArgs`(由 `ApprovalGate::ask_question`
//!   内部校验,这里不用重复)。
//!
//! ## 权限
//!
//! `required_permission = Auto` —— 工具自身不修改任何文件,只是向用户提问。
//! 但**单次调用**会阻塞整个 turn(等用户按键),所以 `is_concurrency_safe = false`,
//! 防止 LLM 在一次响应里多次调用 `ask_user_question` 抢锁。
//!
//! ## 答案 wire 格式
//!
//! `ToolOutput.content[0]` 是 `ContentBlock::Text`,内容是 `AskUserAnswer` 的
//! JSON 字符串(`{"answers":[{"selected":[0],"custom":"..."}, ...]}`)。
//! LLM 解析时拿到结构化答案;人类/调试时看到 JSON 也比散装文本更易读。

use async_trait::async_trait;
use reflect_protocol::{
    AskUserAnswer, ContentBlock, PermissionMode, Question, QuestionOption, ToolOutput,
};
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError};

/// `ask_user_question` 工具。`max_questions` 限制单次调用的问题数量上限,
/// 由 reflect-exec 从 `config.toml [ask_user_question]` 注入(默认 4)。
/// 此前该配置是死信 —— `AskUserQuestionSection.resolved()` 实现完整但
/// 无调用者,问题数量上限硬编码在 protocol 常量,用户配置不生效。
pub struct AskUserQuestionTool {
    pub max_questions: u8,
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self {
            max_questions: reflect_protocol::MAX_QUESTIONS as u8,
        }
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.ask_user_question")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "Short chip label (≤12 chars)",
                                "maxLength": 12
                            },
                            "question": {
                                "type": "string",
                                "description": "The full question text"
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"},
                                        "preview": {"type": ["string", "null"]}
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "Allow selecting multiple options"
                            }
                        },
                        "required": ["header", "question", "options", "multi_select"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })
    }

    /// 不并发 —— 一次 ask 会阻塞整个 turn 等用户按键,并发调用会让
    /// TUI 多个 question modal 抢同一键盘路由,语义不清。
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// `Auto` —— 工具自身无副作用,只是"问用户"的中介。`ApprovalGate`
    /// 内部的 `ask_question` 已经做了校验。
    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        // 1. 解析 + 校验 questions 数组。v1.x:用 `self.max_questions`(从
        //    config.toml [ask_user_question] 注入)校验上限,此前硬编码 4。
        let questions: Vec<Question> = parse_questions(&args, self.max_questions)?;

        // 2. headless 模式 / 无 TUI 时,`ctx.approval = None`,返回错误
        //    而不是无限阻塞 —— LLM 看到 tool failed 后应回退到 plan/ask text。
        let gate = ctx.approval.as_ref().ok_or_else(|| {
            ToolError::Execution(
                "ask_user_question: no ApprovalGate available (headless mode not supported)".into(),
            )
        })?;

        // 3. 同步等待用户回执(oneshot + cancel 短路)。
        let start = std::time::Instant::now();
        let answers: AskUserAnswer = gate.ask_question(questions, &ctx.cancel).await?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        // 4. 序列化为 JSON,塞进 ToolOutput.content 让 LLM 解析。
        let answer_json = serde_json::to_string(&answers).map_err(|e| {
            ToolError::Execution(format!(
                "ask_user_question: failed to serialize answer: {e}"
            ))
        })?;

        Ok(ToolOutput {
            content: vec![ContentBlock::text(answer_json.clone())],
            is_error: false,
            metadata: serde_json::json!({
                "questions_count": answers.answers.len(),
                "elapsed_ms": elapsed_ms,
                "answer_json": answers,
            }),
            elapsed_ms,
        })
    }
}

/// 解析 args 到 `Vec<Question>`,把每条 `Question::new` 的 Result 转 ToolError。
/// `max_questions` 校验问题数量上限(由 config 注入,默认 4)。
fn parse_questions(args: &Value, max_questions: u8) -> Result<Vec<Question>, ToolError> {
    let arr = args
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: "ask_user_question: missing or non-array 'questions'".into(),
        })?;
    if arr.len() > max_questions as usize {
        return Err(ToolError::InvalidArgs {
            message: format!(
                "ask_user_question: too many questions: {} (max {max_questions})",
                arr.len()
            ),
        });
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let header =
            item.get("header")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: questions[{i}].header missing or not a string"
                    ),
                })?;
        let question = item
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!(
                    "ask_user_question: questions[{i}].question missing or not a string"
                ),
            })?;
        let opts_arr = item
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!(
                    "ask_user_question: questions[{i}].options missing or not an array"
                ),
            })?;
        let mut options = Vec::with_capacity(opts_arr.len());
        for (j, opt) in opts_arr.iter().enumerate() {
            let label = opt
                .get("label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: questions[{i}].options[{j}].label missing or not a string"
                    ),
                })?;
            let description = opt
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: questions[{i}].options[{j}].description missing or not a string"
                    ),
                })?;
            let preview = opt
                .get("preview")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            options.push(QuestionOption {
                label: label.to_string(),
                description: description.to_string(),
                preview,
            });
        }
        let multi_select = item
            .get("multi_select")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!(
                    "ask_user_question: questions[{i}].multi_select missing or not a bool"
                ),
            })?;

        // 用 `Question::new` 走统一边界校验(header ≤ 12、options 2-4)。
        let q = Question::new(header, question, options, multi_select).map_err(|e| {
            ToolError::InvalidArgs {
                message: format!("ask_user_question: questions[{i}] invalid: {e}"),
            }
        })?;
        out.push(q);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use reflect_protocol::Event;
    use reflect_protocol::question::{Answer, AskUserAnswer};

    use crate::approval::ApprovalGate;

    fn sample_args() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "header": "Lang",
                    "question": "Pick a language",
                    "options": [
                        {"label": "Rust", "description": "safe + fast"},
                        {"label": "Go", "description": "simple"}
                    ],
                    "multi_select": false
                }
            ]
        })
    }

    fn make_ctx_with_gate() -> (ToolContext, mpsc::Receiver<Event>, Arc<ApprovalGate>) {
        let (tx, rx) = mpsc::channel::<Event>(8);
        let gate = Arc::new(ApprovalGate::new(tx, "sub-test"));
        let ctx = ToolContext {
            approval: Some(gate.clone()),
            cancel: CancellationToken::new(),
            ..Default::default()
        };
        (ctx, rx, gate)
    }

    #[tokio::test]
    async fn parse_questions_valid() {
        let qs = parse_questions(&sample_args(), 4).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].header, "Lang");
        assert_eq!(qs[0].options.len(), 2);
        assert!(!qs[0].multi_select);
    }

    #[tokio::test]
    async fn parse_questions_rejects_missing_array() {
        let err = parse_questions(&serde_json::json!({}), 4).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn parse_questions_rejects_non_bool_multi_select() {
        let bad = serde_json::json!({
            "questions": [{
                "header": "H",
                "question": "Q",
                "options": [
                    {"label": "A", "description": "a"},
                    {"label": "B", "description": "b"}
                ],
                "multi_select": "yes"
            }]
        });
        let err = parse_questions(&bad, 4).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn parse_questions_rejects_long_header() {
        let bad = serde_json::json!({
            "questions": [{
                "header": "verylongheader",  // 15 chars > 12
                "question": "Q",
                "options": [
                    {"label": "A", "description": "a"},
                    {"label": "B", "description": "b"}
                ],
                "multi_select": false
            }]
        });
        let err = parse_questions(&bad, 4).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn execute_rejects_when_no_approval_gate() {
        // headless 模式:ctx.approval = None → ToolError::Execution
        let tool = AskUserQuestionTool::default();
        let ctx = ToolContext::default();
        let err = tool.execute(ctx, sample_args()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn execute_emits_event_and_returns_answer_json() {
        // happy path:tool 发出 AskUserQuestion event,我们从 rx 取出
        // request_id,通过 gate.complete_question 回执答案,tool 返回
        // ToolOutput 含 JSON 答案。
        let tool = AskUserQuestionTool::default();
        let (ctx, mut rx, gate) = make_ctx_with_gate();
        let _cancel_for_spawn = ctx.cancel.clone();
        let args = sample_args();
        let runner = tokio::spawn(async move { tool.execute(ctx, args).await });

        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let request_id = match ev.msg {
            reflect_protocol::EventMsg::AskUserQuestion(e) => e.request_id,
            other => panic!("expected AskUserQuestion, got {other:?}"),
        };

        // 回执:用户选第 0 个 option + "Other" 自定义。
        let answers = AskUserAnswer {
            answers: vec![Answer::single(0).with_custom("Pick Rust")],
        };
        assert!(gate.complete_question(&request_id, answers));

        let out = tokio::time::timeout(Duration::from_secs(2), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!out.is_error);
        // content[0] 是 JSON 文本,包含 "answers" + "selected":[0],"custom":"Pick Rust"。
        match &out.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("answers"), "got: {text}");
                assert!(text.contains("Pick Rust"), "got: {text}");
            }
            _ => panic!("expected text block"),
        }
        // metadata 含 questions_count + elapsed_ms + answer_json。
        assert_eq!(out.metadata["questions_count"], 1);
        assert!(out.metadata["answer_json"]["answers"][0]["custom"] == "Pick Rust");
    }

    #[tokio::test]
    async fn execute_propagates_cancellation() {
        // 用户在 TUI 调 cancel token → tool 收到 ToolError::Cancelled。
        let tool = AskUserQuestionTool::default();
        let (ctx, _rx, _gate) = make_ctx_with_gate();
        let cancel = ctx.cancel.clone();
        let runner = tokio::spawn(async move { tool.execute(ctx, sample_args()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let err = tokio::time::timeout(Duration::from_secs(2), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
    }

    #[tokio::test]
    async fn tool_metadata_is_stable() {
        // 协议级元数据稳定契约。
        let t = AskUserQuestionTool::default();
        assert_eq!(t.name(), "ask_user_question");
        assert!(!t.is_concurrency_safe(), "ask_user_question 必须串行");
        assert_eq!(t.required_permission(), PermissionMode::Auto);
        let schema = t.parameters_schema();
        assert_eq!(schema["required"][0], "questions");
        // questions 数组的 minItems / maxItems 限制。
        assert_eq!(schema["properties"]["questions"]["minItems"], 1);
        assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
    }
}
