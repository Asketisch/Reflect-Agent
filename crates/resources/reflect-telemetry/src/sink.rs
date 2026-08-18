//! [`TelemetrySink`] —— 引擎层消费 telemetry 的高级接口。
//!
//! 持有一个 [`SpanLogWriter`] + 一个 [`ModelIoWriter`](按 session),
//! 提供 `record_turn_started` / `record_turn_completed` /
//! `record_model_call` / `record_tool_call` / `record_goal_event` 等方法。
//!
//! 设计要点:
//! - `enabled = false` 时所有方法是 no-op(`None` sink 也可)。
//! - 所有写入 best-effort,绝不返回 Result 给引擎(失败只 warn,
//!   对齐 rollout 的哲学:telemetry 失败不能中断在途 turn)。
//! - `Arc<TelemetrySink>` 可安全跨 `tokio::spawn` 共享。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::model::{
    DEFAULT_TRACES_DIR, Level, ModelIoRecord, ModelRef, SpanStatus, TraceEvent, UsageSnapshot,
    new_span_id, new_trace_id,
};
use crate::writer::{ModelIoWriter, SpanLogWriter};

/// 当前 turn 的 span 上下文(RAII guard 在 drop 时写 completed 事件)。
///
/// 引擎在 turn 开始时 `sink.enter_turn(...)` 拿到一个 guard,
/// guard drop 时(无论正常结束还是 panic)自动写 `turn.completed` 事件
/// 带 duration。引擎也可以显式调 `guard.complete(status)` 提前写。
pub struct TurnSpan {
    trace_id: String,
    session_id: String,
    turn_id: String,
    span_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    /// 共享同一 sink(Arc clone,与引擎持有的是同一个)。
    sink: Arc<TelemetrySink>,
    /// 是否已写过 completed(避免 drop 时重复写)。
    completed: Mutex<bool>,
}

impl TurnSpan {
    /// 显式写一条 `turn.completed`(带 status + context)。
    pub fn complete(&self, status: SpanStatus, context: serde_json::Value) {
        let mut done = self.completed.lock();
        if *done {
            return;
        }
        *done = true;
        drop(done);
        self.emit_completed(status, context);
    }

    fn emit_completed(&self, status: SpanStatus, context: serde_json::Value) {
        if !self.sink.enabled() {
            return;
        }
        let duration_ms = Utc::now()
            .signed_duration_since(self.started_at)
            .num_milliseconds()
            .max(0) as u64;
        let ev = TraceEvent::turn_completed(
            &self.trace_id,
            &self.session_id,
            &self.turn_id,
            &self.span_id,
            duration_ms,
            status,
            context,
        );
        self.sink.emit_span(&ev);
    }
}

impl Drop for TurnSpan {
    fn drop(&mut self) {
        let done = *self.completed.lock();
        if !done {
            // 未显式 complete → 默认标记 completed(兜底,防漏写)。
            self.emit_completed(
                SpanStatus::Completed,
                serde_json::json!({"note": "auto-completed on drop"}),
            );
        }
    }
}

/// 引擎层的 telemetry 汇聚点。
///
/// 一个 session 对应一个 `TelemetrySink`(因 ModelIoWriter 按 session 绑定)。
/// 通过 [`TelemetrySink::new`] 构造,默认 `enabled = true`。
pub struct TelemetrySink {
    enabled: bool,
    trace_id: String,
    session_id: String,
    span_log: SpanLogWriter,
    model_io: ModelIoWriter,
    /// P2 `langfuse`:可选的 cloud exporter,与本地 JSONL 并行投递。
    /// `None` = 仅本地日志(默认)。注入后所有 span 事件同时 forward 一份。
    langfuse: Mutex<Option<Arc<crate::langfuse::LangfuseExporter>>>,
}

impl TelemetrySink {
    /// 构造一个绑定到 `base_dir` + `session_id` 的 sink。
    /// `trace_id` 由本方法生成(= session 级根 trace id)。
    pub fn new(base_dir: impl Into<PathBuf>, session_id: impl Into<String>) -> Arc<Self> {
        let session_id = session_id.into();
        let base_dir = base_dir.into();
        let trace_id = new_trace_id();
        Arc::new(Self {
            enabled: true,
            trace_id,
            session_id: session_id.clone(),
            span_log: SpanLogWriter::new(base_dir.clone()),
            model_io: ModelIoWriter::new(base_dir, session_id),
            langfuse: Mutex::new(None),
        })
    }

    /// 禁用的 sink(所有方法 no-op)。用于 `[telemetry] enabled = false`。
    pub fn disabled() -> Arc<Self> {
        let dummy = PathBuf::from("/dev/null");
        Arc::new(Self {
            enabled: false,
            trace_id: String::new(),
            session_id: String::new(),
            span_log: SpanLogWriter::new(dummy.clone()),
            model_io: ModelIoWriter::new(dummy, ""),
            langfuse: Mutex::new(None),
        })
    }

    /// P2 `langfuse`:注入 cloud exporter,后续 span 事件同时投递到本地 + Langfuse。
    pub fn set_langfuse(&self, exporter: Arc<crate::langfuse::LangfuseExporter>) {
        *self.langfuse.lock() = Some(exporter);
    }

    /// 把一条 span 事件同时写本地 + forward 到 langfuse(若有)。
    fn emit_span(&self, ev: &TraceEvent) {
        self.span_log.append(ev);
        if let Some(exp) = self.langfuse.lock().as_ref() {
            exp.send(ev.clone());
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 进入一个 turn span,返回 RAII guard。
    /// guard drop 时自动写 `turn.completed`。
    ///
    /// 调用方需持有 sink 的 `Arc`(通常 `sink.enter_turn(...)` 在 `&Arc<TelemetrySink>`
    /// 上调用;这里返回的 guard 持有 sink 的一个 clone,但 guard 不暴露 sink
    /// 访问器,故不存在借用问题)。
    pub fn enter_turn(self: &Arc<Self>, turn_id: &str) -> TurnSpan {
        let span_id = new_span_id();
        if self.enabled {
            let ev = TraceEvent::turn_started(&self.trace_id, &self.session_id, turn_id, &span_id);
            self.emit_span(&ev);
        }
        TurnSpan {
            trace_id: self.trace_id.clone(),
            session_id: self.session_id.clone(),
            turn_id: turn_id.to_string(),
            span_id,
            started_at: Utc::now(),
            sink: Arc::clone(self),
            completed: Mutex::new(false),
        }
    }

    /// 记录一次完整的 LLM 调用(写 model-io + 一条 model.request.completed span)。
    ///
    /// `parent_span_id` 通常是 turn span 的 id。
    ///
    /// 参数刻意保持「一次调用一个平铺签名」—— 调用点(model_call / summarizer /
    /// verifier)都是 best-effort 落库,平铺比「构造参数结构体再传」更直白。
    #[allow(clippy::too_many_arguments)]
    pub fn record_model_call(
        &self,
        parent_span_id: Option<&str>,
        turn_id: Option<&str>,
        model: ModelRef,
        request: serde_json::Value,
        response: serde_json::Value,
        usage: UsageSnapshot,
        duration_ms: u64,
        attempt: u32,
        status: SpanStatus,
        query_source: &str,
    ) {
        if !self.enabled {
            return;
        }
        let request_id = Uuid::new_v4().to_string();
        let span_id = new_span_id();
        let now = Utc::now();
        // model-io 完整记录。
        let rec = ModelIoRecord {
            started_at: now - chrono::Duration::milliseconds(duration_ms as i64),
            completed_at: now,
            duration_ms,
            attempt,
            request_id: request_id.clone(),
            trace_id: self.trace_id.clone(),
            turn_id: turn_id.map(String::from),
            session_id: self.session_id.clone(),
            query_source: query_source.into(),
            model: model.clone(),
            request,
            response: response.clone(),
            usage: usage.clone(),
        };
        self.model_io.append(&rec);
        // span 事件(精简 context,token/cost 给 TUI 用)。
        let context = serde_json::json!({
            "model": model.model_id,
            "provider": model.provider_id,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "total_tokens": usage.total_tokens,
                "cost_usd": usage.cost_usd,
            },
            "finish_reason": response.get("finish_reason"),
        });
        let ev = TraceEvent {
            timestamp: now,
            level: if status == SpanStatus::Failed {
                Level::Error
            } else {
                Level::Info
            },
            event: if status == SpanStatus::Failed {
                "model.request.failed"
            } else {
                "model.request.completed"
            }
            .into(),
            module: "reflect_core::graph::nodes".into(),
            trace_id: self.trace_id.clone(),
            span_id,
            parent_span_id: parent_span_id.map(String::from),
            session_id: Some(self.session_id.clone()),
            turn_id: turn_id.map(String::from),
            tool_call_id: None,
            status: Some(status),
            duration_ms: Some(duration_ms),
            context: Some(context),
        };
        self.emit_span(&ev);
    }

    /// 记录一次工具调用(写一条 tool.call.ended span)。
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_call(
        &self,
        parent_span_id: Option<&str>,
        turn_id: Option<&str>,
        call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        output_truncated: &str,
        is_error: bool,
        elapsed_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let span_id = new_span_id();
        let context = serde_json::json!({
            "tool": tool_name,
            "args": args,
            "output_preview": output_truncated.chars().take(512).collect::<String>(),
            "is_error": is_error,
        });
        let ev = TraceEvent {
            timestamp: Utc::now(),
            level: if is_error { Level::Warn } else { Level::Info },
            event: if is_error {
                "tool.call.failed"
            } else {
                "tool.call.ended"
            }
            .into(),
            module: "reflect_core::graph::nodes".into(),
            trace_id: self.trace_id.clone(),
            span_id,
            parent_span_id: parent_span_id.map(String::from),
            session_id: Some(self.session_id.clone()),
            turn_id: turn_id.map(String::from),
            tool_call_id: Some(call_id.into()),
            status: Some(if is_error {
                SpanStatus::Failed
            } else {
                SpanStatus::Completed
            }),
            duration_ms: Some(elapsed_ms),
            context: Some(context),
        };
        self.emit_span(&ev);
    }

    /// 记录一个 goal 事件(Work 2 复用)。
    pub fn record_goal_event(
        &self,
        turn_id: Option<&str>,
        event: &str,
        level: Level,
        context: serde_json::Value,
    ) {
        if !self.enabled {
            return;
        }
        let span_id = new_span_id();
        let ev = TraceEvent {
            timestamp: Utc::now(),
            level,
            event: event.into(),
            module: "reflect_goal".into(),
            trace_id: self.trace_id.clone(),
            span_id,
            parent_span_id: None,
            session_id: Some(self.session_id.clone()),
            turn_id: turn_id.map(String::from),
            tool_call_id: None,
            status: None,
            duration_ms: None,
            context: Some(context),
        };
        self.emit_span(&ev);
    }
}

/// 解析 `$HOME` 下的默认 trace 目录。供引擎构造 sink 用。
pub fn resolve_traces_dir(config_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = config_dir {
        return dir.join("traces");
    }
    // fallback: $HOME/.reflect/traces
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(DEFAULT_TRACES_DIR);
    }
    PathBuf::from(DEFAULT_TRACES_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn disabled_sink_is_noop() {
        let sink = TelemetrySink::disabled();
        assert!(!sink.enabled());
        // 所有方法都不应 panic / 写文件。
        sink.record_model_call(
            None,
            None,
            ModelRef::default(),
            serde_json::json!({}),
            serde_json::json!({}),
            UsageSnapshot::default(),
            100,
            1,
            SpanStatus::Completed,
            "main_turn",
        );
        sink.record_tool_call(
            None,
            None,
            "c1",
            "Bash",
            &serde_json::json!({}),
            "ok",
            false,
            10,
        );
    }

    #[test]
    fn enabled_sink_writes_turn_started_and_completed() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-test");
        let turn_id = "turn-1";
        {
            let _guard = sink.enter_turn(turn_id);
            // guard drop 时写 completed
            let _ = &sink; // keep sink alive past guard
        }
        let events = crate::writer::list_span_log_files(dir.path(), 1);
        assert_eq!(events.len(), 1);
        let evs = crate::writer::read_span_log_file(&events[0].1);
        assert_eq!(evs.len(), 2, "expected started + completed, got {evs:?}");
        assert_eq!(evs[0].event, "turn.started");
        assert_eq!(evs[1].event, "turn.completed");
    }

    #[test]
    fn record_model_call_writes_model_io_and_span() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-mio");
        sink.record_model_call(
            None,
            Some("turn-1"),
            ModelRef {
                model_id: "glm-5.2".into(),
                provider_id: Some("builtin".into()),
                role: Some("main".into()),
                source: Some("main_turn".into()),
            },
            serde_json::json!({"message_count": 3}),
            serde_json::json!({"finish_reason": "stop"}),
            UsageSnapshot {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                cost_usd: Some(0.002),
                ..Default::default()
            },
            500,
            1,
            SpanStatus::Completed,
            "main_turn",
        );
        // span log 有一条 model.request.completed。
        let files = crate::writer::list_span_log_files(dir.path(), 1);
        let evs = crate::writer::read_span_log_file(&files[0].1);
        assert!(
            evs.iter().any(|e| e.event == "model.request.completed"),
            "missing model.request.completed in {evs:?}"
        );
        // model-io 有一条记录。
        let mio_path = dir
            .path()
            .join("model-io")
            .join("model-io-sess_sess-mio.jsonl");
        let recs = crate::writer::read_model_io_file(&mio_path);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].usage.input_tokens, 100);
    }

    #[test]
    fn record_tool_call_writes_span() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-tc");
        sink.record_tool_call(
            None,
            Some("turn-1"),
            "call-1",
            "Bash",
            &serde_json::json!({"command": "ls"}),
            "file1\nfile2",
            false,
            42,
        );
        let files = crate::writer::list_span_log_files(dir.path(), 1);
        let evs = crate::writer::read_span_log_file(&files[0].1);
        assert!(
            evs.iter().any(
                |e| e.event == "tool.call.ended" && e.tool_call_id.as_deref() == Some("call-1")
            ),
            "missing tool.call.ended in {evs:?}"
        );
    }
}
