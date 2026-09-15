//! Goal 校验器 —— LLM 自校验(参考 zcode `target_completion_verification` +
//! 严格基于证据的校验 prompt)。
//!
//! 发起一次独立 LLM 调用(query_source = `goal_verification`),prompt 强制
//! 结构化 JSON 输出 `{verdict, evidence/remaining/reason}`,解析为 [`GoalVerdict`]。
//!
//! prompt 核心(参考设计规范):
//! > 只当每条需求都有**当前**证据支持才标 met;证据不完整 / 间接 / 仅
//! > consistent → 标 unmet 继续工作;遇到外部阻塞标 blocked。
//!
//! 可选命令校验(复用 reflect-hooks verification 的 run_command 思路):
//! exit 0 作为 AND 条件 —— LLM 判 met 且命令通过才算真完成。

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use reflect_llm::client::ModelClient;
use reflect_llm::request::{ChatRequest, SystemBlock, SystemBlocks};
use reflect_llm::{ChatMessage, ContentBlock, UserContent};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::state::GoalVerdict;

/// verify_command 的超时(对齐 reflect-hooks VerificationHook 的 5 分钟)。
const VERIFY_CMD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 校验一个目标是否达成。发起 LLM 自校验(必须),可选跑 verify_command。
///
/// `work_context` 是本轮 agent 的工作摘要(assistant 的最后文本),供
/// verifier 参考「做了什么」。`client` 是校验用的 LLM client(可与主 agent
/// 不同,如指定更便宜的模型)。`telemetry` 非空时落 model-io 记录
/// (query_source = `goal_verification`)。
pub async fn verify(
    goal: &str,
    work_context: &str,
    client: &dyn ModelClient,
    cancel: &CancellationToken,
    telemetry: Option<&Arc<reflect_telemetry::TelemetrySink>>,
) -> Result<GoalVerdict, VerifyError> {
    let prompt = build_verification_prompt(goal, work_context);
    let request = ChatRequest {
        model: String::new(), // client 内部已知自己的 model
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Text { text: prompt }],
        })],
        system: SystemBlocks(vec![SystemBlock {
            text: VERIFIER_SYSTEM_PROMPT.to_string(),
            cache_control: None,
            ephemeral: false,
        }]),
        tools: vec![],
        // v1.4 B1/B2:声明 JsonObject 结构化输出 + 走非流式便捷接口。
        // provider 支持时由服务端约束 JSON(Anthropic 侧经内部强制工具
        // 透明实现);不支持/解析失败的容错仍由 `parse_verdict` 的
        // extract 逻辑兜底 —— 双保险,行为向后兼容。
        response_format: Some(reflect_llm::ResponseFormat::JsonObject),
        ..Default::default()
    };
    // v1.2 P1:整体调用计时 + provider 名(供落库)。
    let call_started = std::time::Instant::now();
    let provider = client.name().to_string();
    // v1.4 B2:非流式收集委托给 `ModelClient::complete`(默认实现内部
    // 走 stream 拼接 delta 并捕获最后 Usage 快照)—— 校验器不再自带
    // 一份流收集循环。中途流错误在 complete 内部即为 Err。
    let out = client
        .complete(request.clone(), cancel.clone())
        .await
        .map_err(|e| VerifyError::Llm(e.to_string()))?;
    let text = out.text;
    // v1.2 P1:落库(若有 sink)。verifier 用 &dyn ModelClient 无法获知
    // 具体 model 名,model_id 留空(provider 仍记录 client.name())。
    if let Some(sink) = telemetry
        && sink.enabled()
    {
        let model_ref = reflect_telemetry::ModelRef {
            model_id: String::new(),
            provider_id: Some(provider.clone()),
            role: Some("goal".into()),
            source: Some("goal_verification".into()),
        };
        let req_record = serde_json::json!({
            "messages": serde_json::to_value(&request.messages).unwrap_or(serde_json::Value::Null),
            "system": serde_json::to_value(&request.system).unwrap_or(serde_json::Value::Null),
        });
        let resp_record = serde_json::json!({
            "finish_reason": "stop",
            "text": text,
        });
        let usage = out.usage.unwrap_or_default();
        let (usage_input, usage_output) = (usage.input_tokens as u64, usage.output_tokens as u64);
        sink.record_model_call(
            None,
            None,
            model_ref,
            req_record,
            resp_record,
            reflect_telemetry::UsageSnapshot {
                input_tokens: usage_input,
                output_tokens: usage_output,
                cached_tokens: usage.cached_tokens as u64,
                cache_write_tokens: usage.cache_write_tokens as u64,
                total_tokens: usage_input + usage_output,
                cost_usd: None,
            },
            call_started.elapsed().as_millis() as u64,
            1,
            reflect_telemetry::SpanStatus::Completed,
            "goal_verification",
        );
    }
    parse_verdict(&text)
}

/// 跑 verify_command,返回 exit 是否 0(= passed)。失败/超时 → false。
pub async fn run_verify_command(cmd: &str) -> bool {
    if cmd.trim().is_empty() {
        return true;
    }
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, cmd, "goal verify command spawn failed");
            return false;
        }
    };
    // 排空管道防阻塞(verifier 只看 exit code,内容丢弃)。
    //
    // 此前用 `take(100 KiB).read_to_end(&mut Vec::new())` 有两个死锁路径:
    // 1. `take(100 KiB)` 只读 100 KiB 即返回 EOF,但底层 ChildStdout 管道仍
    //    可能有数据 —— 超过 100 KiB 的输出会让子进程的 write 阻塞(管道缓冲
    //    区 ~64 KiB 写满),进而 `child.wait()` 永久挂起,直到 5 分钟超时。
    // 2. stdout / stderr 顺序读取:stdout 未读完时,若 stderr 也写满,子进程
    //    同样阻塞在 write → wait 挂起。
    //
    // 正确做法:不限量把两端管道读到底(读到真正的 EOF = 子进程关闭写端),
    // 并用 `tokio::join!` 并发排空 stdout + stderr,避免任一管道积压。
    let collect = async {
        let stdout_fut = async {
            if let Some(mut s) = child.stdout.take() {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf).await;
            }
        };
        let stderr_fut = async {
            if let Some(mut s) = child.stderr.take() {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf).await;
            }
        };
        let ((), ()) = tokio::join!(stdout_fut, stderr_fut);
        child.wait().await
    };
    match timeout(VERIFY_CMD_TIMEOUT, collect).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "goal verify command wait failed");
            false
        }
        Err(_) => {
            tracing::warn!("goal verify command timed out");
            false
        }
    }
}

/// verifier 的 system prompt —— 强制 evidence-based, requirement-by-requirement。
/// 采用严格措辞。
const VERIFIER_SYSTEM_PROMPT: &str = "\
You are a strict goal-completion verifier. You judge whether a coding goal \
has been FULLY achieved based on the work performed.\n\n\
CRITICAL RULES:\n\
1. Only mark the goal `met` when EVERY requirement has current, direct evidence \
of completion. Inspect the actual work output, not the agent's claims.\n\
2. If evidence is incomplete, weak, indirect, merely consistent with completion, \
or leaves ANY requirement missing/incomplete/unverified → mark `unmet`.\n\
3. Mark `blocked` ONLY for external blockers (missing permissions, unavailable \
dependencies, environment issues) — NOT for hard work you think is too difficult.\n\
4. Respond as STRICT JSON, no prose outside the JSON object.";

/// 构造 user prompt,要求结构化 JSON 输出。
fn build_verification_prompt(goal: &str, work_context: &str) -> String {
    format!(
        "\
<goal>{goal}</goal>\n\n\
<latest_work>\n{work_context}\n</latest_work>\n\n\
Judge whether the goal above is fully achieved based on the work performed. \
Respond as a single JSON object with this exact shape:\n\
- {{\"verdict\":\"met\",\"evidence\":[\"req1: <proof>\",\"req2: <proof>\"]}}\n\
- {{\"verdict\":\"unmet\",\"remaining\":[\"<what still needs doing>\"]}}\n\
- {{\"verdict\":\"blocked\",\"reason\":\"<external blocker>\"}}\n\n\
JSON only, no markdown fences, no explanation outside the object."
    )
}

/// 解析 LLM 返回的 JSON 为 GoalVerdict。容错:提取首个 {...} 块再解析。
fn parse_verdict(text: &str) -> Result<GoalVerdict, VerifyError> {
    let json_str = extract_json_object(text);
    let v: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| VerifyError::Parse(format!("invalid JSON: {e}; raw: {text}")))?;
    let verdict = v
        .get("verdict")
        .and_then(|x| x.as_str())
        .ok_or_else(|| VerifyError::Parse(format!("missing 'verdict' field; raw: {text}")))?;
    match verdict {
        "met" => {
            let evidence = v
                .get("evidence")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(GoalVerdict::Met { evidence })
        }
        "unmet" => {
            let remaining = v
                .get("remaining")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(GoalVerdict::Unmet { remaining })
        }
        "blocked" => {
            let reason = v
                .get("reason")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown blocker")
                .to_string();
            Ok(GoalVerdict::Blocked { reason })
        }
        other => Err(VerifyError::Parse(format!(
            "unknown verdict '{other}'; raw: {text}"
        ))),
    }
}

/// 从可能含 markdown fence / 前后噪声的文本里提取首个 {...} 块。
fn extract_json_object(text: &str) -> String {
    let trimmed = text.trim();
    // 去掉 ```json ... ``` fence。
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim()
        .trim_end_matches("```")
        .trim();
    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}'))
        && start <= end
    {
        return stripped[start..=end].to_string();
    }
    stripped.to_string()
}

/// 校验错误。
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("LLM stream init failed: {0}")]
    StreamInit(String),
    #[error("LLM error during verification: {0}")]
    Llm(String),
    #[error("verification cancelled")]
    Cancelled,
    #[error("verdict parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_met_verdict() {
        let v =
            parse_verdict(r#"{"verdict":"met","evidence":["tests pass","lint clean"]}"#).unwrap();
        match v {
            GoalVerdict::Met { evidence } => assert_eq!(evidence.len(), 2),
            _ => panic!("expected met"),
        }
    }

    #[test]
    fn parse_unmet_verdict() {
        let v = parse_verdict(r#"{"verdict":"unmet","remaining":["fix test_auth","add docs"]}"#)
            .unwrap();
        match v {
            GoalVerdict::Unmet { remaining } => assert_eq!(remaining.len(), 2),
            _ => panic!("expected unmet"),
        }
    }

    #[test]
    fn parse_blocked_verdict() {
        let v = parse_verdict(r#"{"verdict":"blocked","reason":"no DB access"}"#).unwrap();
        match v {
            GoalVerdict::Blocked { reason } => assert_eq!(reason, "no DB access"),
            _ => panic!("expected blocked"),
        }
    }

    #[test]
    fn parse_strips_markdown_fence() {
        let raw = "```json\n{\"verdict\":\"met\",\"evidence\":[]}\n```";
        let v = parse_verdict(raw).unwrap();
        assert!(v.is_met());
    }

    #[test]
    fn parse_extracts_json_from_noise() {
        let raw = "Here is my judgment:\n{\"verdict\":\"unmet\",\"remaining\":[\"x\"]}\nThanks!";
        let v = parse_verdict(raw).unwrap();
        assert!(matches!(v, GoalVerdict::Unmet { .. }));
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(parse_verdict("not json at all").is_err());
        assert!(parse_verdict(r#"{"verdict":"wat"}"#).is_err());
    }

    #[tokio::test]
    async fn run_verify_command_empty_is_pass() {
        assert!(run_verify_command("").await);
        assert!(run_verify_command("   ").await);
    }

    #[tokio::test]
    async fn run_verify_command_exit_zero() {
        assert!(run_verify_command("true").await);
    }

    #[tokio::test]
    async fn run_verify_command_exit_nonzero() {
        assert!(!run_verify_command("false").await);
    }
}
