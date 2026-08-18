//! Telemetry 数据模型 —— 对齐 zcode 的本地日志两层结构。
//!
//! 参考 zcode(`~/.zcode/cli/log/zcode-YYYY-MM-DD.jsonl` 与
//! `~/.zcode/cli/rollout/model-io-sess_<id>.jsonl`)的 schema:
//! - [`TraceEvent`] 是一条 JSONL span 事件(turn/model/tool 级别),
//!   带 `trace_id` / `span_id` / `parent_span_id` 父子链,支持 span 树。
//! - [`ModelIoRecord`] 是一次完整 LLM 调用的请求/响应/usage/cost/latency,
//!   相当于 Langfuse 的 `generation` 记录。
//!
//! 两类记录都按 zcode 的 key 元组 `(trace_id, session_id, turn_id)` 关联,
//! 便于跨文件 join 与 `/traces` TUI overlay 按会话过滤浏览。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 默认 trace 根目录(位于 `$HOME` 下)。
pub const DEFAULT_TRACES_DIR: &str = ".reflect/traces";

/// span 日志子目录(按天轮转的 `reflect-YYYY-MM-DD.jsonl`)。
pub const SPAN_LOG_SUBDIR: &str = "log";

/// model-io 子目录(按 session 一个 `model-io-sess_<id>.jsonl`)。
pub const MODEL_IO_SUBDIR: &str = "model-io";

/// 单文件轮转阈值(对齐 rollout `ROTATE_AFTER_BYTES = 256 KiB`)。
pub const ROTATE_AFTER_BYTES: u64 = 256 * 1024;

/// 最多保留的轮转副本数(对齐 rollout `MAX_ROTATED_FILES = 3`)。
pub const MAX_ROTATED_FILES: usize = 3;

/// 单个 JSON 字符串字段的截断阈值(对齐 rollout `MAX_JSONL_FIELD_CHARS = 16 KiB`)。
pub const MAX_FIELD_CHARS: usize = 16 * 1024;

/// 截断后追加的标记。
pub const REDACTION_MARKER: &str = "[redacted]";

/// span id 的字符前缀长度(对齐 zcode 的 13-char 前缀 uuid)。
pub const SPAN_ID_LEN: usize = 13;

/// span 事件级别(对齐 zcode 的 `level` 字段)。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// span 状态。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpanStatus {
    Started,
    #[default]
    Completed,
    Failed,
}

impl SpanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpanStatus::Started => "started",
            SpanStatus::Completed => "completed",
            SpanStatus::Failed => "failed",
        }
    }
}

/// 一次 LLM 调用的 token 用量快照(独立于 protocol 的 `TokenUsage`,
/// 保留 u64 以容纳聚合场景;本地模型 Ollama 已返回真实计数)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 模型引用(provider/model/role/source),对齐 zcode `model-io` 的 `model` 字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRef {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 调用来源:`main_turn` / `subagent` / `compact` / `goal_verification`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// 一条 span 事件(一行 JSONL)。对齐 zcode Store A 的 envelope:
/// `timestamp / level / event / module / traceId / spanId / parentSpanId /
/// sessionId / turnId / toolCallId / status / durationMs / context`。
///
/// `span_id` / `parent_span_id` 是 13-char 前缀 uuid,其余 id 是完整字符串。
/// 根 span 的 `parent_span_id = None`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    /// 事件判别器,如 `turn.started` / `model.request.completed` /
    /// `tool.call.ended` / `goal.turn.verified`。
    pub event: String,
    /// 发射模块名(如 `reflect_core::submission_loop`)。
    pub module: String,
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SpanStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 事件特定 payload(usage / verdict / error 等),已脱敏。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// 一次完整 LLM 调用的请求/响应记录(一行 JSONL)。对齐 zcode Store B
/// 的 `model-io` schema:timing / model / request / response / usage。
///
/// 这是 Langfuse `generation` 记录的本地等价物 —— 含完整 prompt 概要 +
/// 响应 + token/cost,便于离线分析与 `/traces` overlay 详情展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelIoRecord {
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// 重试序号(从 1 起,对齐 zcode `attempt`)。
    pub attempt: u32,
    pub request_id: String,
    pub trace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub session_id: String,
    /// 调用来源,同 [`ModelRef::source`]。
    pub query_source: String,
    pub model: ModelRef,
    /// 简化的请求体(`{model, tool_names, message_count, system_summary}`),
    /// 已脱敏 + 截断。完整 messages 体量过大,只保留元信息 + tool 名。
    pub request: serde_json::Value,
    /// 简化的响应体(`{finish_reason, text(截断), tool_calls, usage}`),
    /// 已脱敏。
    pub response: serde_json::Value,
    pub usage: UsageSnapshot,
}

impl TraceEvent {
    /// 构造一条 turn 根 span 的 `started` 事件。
    pub fn turn_started(trace_id: &str, session_id: &str, turn_id: &str, span_id: &str) -> Self {
        Self {
            timestamp: Utc::now(),
            level: Level::Info,
            event: "turn.started".into(),
            module: "reflect_core::submission_loop".into(),
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            tool_call_id: None,
            status: Some(SpanStatus::Started),
            duration_ms: None,
            context: None,
        }
    }

    /// 构造一条 turn 根 span 的 `completed` 事件(duration 由调用方算)。
    pub fn turn_completed(
        trace_id: &str,
        session_id: &str,
        turn_id: &str,
        span_id: &str,
        duration_ms: u64,
        status: SpanStatus,
        context: serde_json::Value,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            level: if status == SpanStatus::Failed {
                Level::Warn
            } else {
                Level::Info
            },
            event: "turn.completed".into(),
            module: "reflect_core::submission_loop".into(),
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            tool_call_id: None,
            status: Some(status),
            duration_ms: Some(duration_ms),
            context: Some(context),
        }
    }
}

/// 生成一个 13-char 前缀的 span id(对齐 zcode)。
pub fn new_span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..SPAN_ID_LEN].to_string()
}

/// 生成一个完整 trace id(= 完整 uuid 字符串,对齐 zcode `traceId`)。
pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 对一个 JSON value 做递归脱敏。两道关卡,顺序固定:
///
/// 1. **密钥 pattern 脱敏**(`reflect_sanitize::redact_string`,10 类默认
///    pattern)—— 合规边界,避免工具 `args` / 模型请求 / 响应里的密钥
///    落盘到本地 trace / ModelIo 日志,或被 Langfuse 上报。
/// 2. **长度截断**(超长字符串截断到 `MAX_FIELD_CHARS`)。
///
/// 对齐 rollout `redact_value`,两处行为一致。
pub fn redact_value(v: &mut serde_json::Value) {
    use reflect_sanitize::redact_string;
    match v {
        serde_json::Value::String(s) => {
            *s = redact_string(s);
            if s.len() > MAX_FIELD_CHARS {
                let cut = MAX_FIELD_CHARS.saturating_sub(REDACTION_MARKER.len());
                s.truncate(cut);
                s.push_str(REDACTION_MARKER);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                redact_value(val);
            }
        }
        _ => {}
    }
}

/// 脱敏后序列化为单行 JSON 字符串。
pub fn serialize_redacted<T: Serialize>(v: &T) -> serde_json::Result<String> {
    let mut val = serde_json::to_value(v)?;
    redact_value(&mut val);
    serde_json::to_string(&val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_id_is_13_chars() {
        let id = new_span_id();
        assert_eq!(id.len(), SPAN_ID_LEN, "got: {id}");
    }

    #[test]
    fn trace_id_is_full_uuid() {
        let id = new_trace_id();
        // 简单 uuid v4 = 36 字符(含连字符)
        assert_eq!(id.len(), 36, "got: {id}");
    }

    #[test]
    fn redact_truncates_over_threshold() {
        let mut v = serde_json::json!("x".repeat(20_000));
        redact_value(&mut v);
        assert!(v.as_str().unwrap().ends_with(REDACTION_MARKER));
    }

    #[test]
    fn redact_preserves_short_strings() {
        let mut v = serde_json::json!("hello");
        redact_value(&mut v);
        assert_eq!(v, serde_json::json!("hello"));
    }

    #[test]
    fn redact_strips_secrets_in_context() {
        // 模拟 record_tool_call 写入的 context(args 含密钥)。
        let mut v = serde_json::json!({
            "tool": "bash",
            "args": { "cmd": "export TOKEN=AKIAIOSFODNN7EXAMPLE" },
            "output_preview": "Authorization: Bearer eyJabc.def.ghi"
        });
        redact_value(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "aws key leaked: {s}");
        assert!(s.contains("[REDACTED:aws_key]"));
        assert!(!s.contains("eyJabc.def.ghi"));
        assert!(s.contains("Bearer [REDACTED]"));
    }

    #[test]
    fn redact_strips_secret_in_model_io_request() {
        let mut v = serde_json::json!(format!(
            "POST /v1/chat key=sk-ant-api03-{}",
            "abcdefghijklmnopqrstuvwxyz"
        ));
        redact_value(&mut v);
        let s = v.as_str().unwrap();
        assert!(!s.contains("sk-ant-api03-abcdefghijklmnopqrstuvwxyz"));
        assert!(s.contains("[REDACTED:anthropic_key]"));
    }

    #[test]
    fn trace_event_serde_has_event_tag() {
        let ev = TraceEvent::turn_started("t1", "s1", "turn1", "span1234567");
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""event":"turn.started""#), "got: {j}");
        assert!(j.contains(r#""trace_id":"t1""#));
        assert!(j.contains(r#""span_id":"span1234567""#));
        assert!(j.contains(r#""status":"started""#));
    }

    #[test]
    fn model_io_record_serde_roundtrip() {
        let r = ModelIoRecord {
            started_at: Utc::now(),
            completed_at: Utc::now(),
            duration_ms: 1234,
            attempt: 1,
            request_id: "req-1".into(),
            trace_id: "t1".into(),
            turn_id: Some("turn1".into()),
            session_id: "s1".into(),
            query_source: "main_turn".into(),
            model: ModelRef {
                model_id: "glm-5.2".into(),
                provider_id: Some("builtin".into()),
                role: Some("main".into()),
                source: Some("main_turn".into()),
            },
            request: serde_json::json!({"message_count": 5, "tool_names": ["Bash","Read"]}),
            response: serde_json::json!({"finish_reason": "tool-calls"}),
            usage: UsageSnapshot {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                cost_usd: Some(0.001),
                ..Default::default()
            },
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains(r#""query_source":"main_turn""#));
        let back: ModelIoRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back.duration_ms, 1234);
    }

    #[test]
    fn serialize_redacted_truncates_context() {
        let ev = TraceEvent {
            timestamp: Utc::now(),
            level: Level::Info,
            event: "test".into(),
            module: "m".into(),
            trace_id: "t".into(),
            span_id: "s".into(),
            parent_span_id: None,
            session_id: None,
            turn_id: None,
            tool_call_id: None,
            status: None,
            duration_ms: None,
            context: Some(serde_json::json!({"big": "y".repeat(20_000)})),
        };
        let line = serialize_redacted(&ev).unwrap();
        assert!(line.len() < 20_000, "line too long: {}", line.len());
        assert!(line.contains(REDACTION_MARKER));
    }
}
