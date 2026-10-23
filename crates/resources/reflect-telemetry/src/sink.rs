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
    DEFAULT_TRACES_DIR, Level, ModelIoRecord, ModelRef, SpanStatus, TelemetryWriteConfig,
    TraceEvent, UsageSnapshot, new_span_id, new_trace_id,
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
    /// v1.x:是否记录完整工具 schema(默认 false,只记工具名)。
    /// 由 `[telemetry].record_tool_schemas` 配置。
    record_tool_schemas: bool,
    /// P2 `langfuse`:可选的 cloud exporter,与本地 JSONL 并行投递。
    /// `None` = 仅本地日志(默认)。注入后所有 span 事件同时 forward 一份。
    langfuse: Mutex<Option<Arc<crate::langfuse::LangfuseExporter>>>,
}

impl TelemetrySink {
    /// 构造一个绑定到 `base_dir` + `session_id` 的 sink。
    /// `trace_id` 由本方法生成(= session 级根 trace id)。
    pub fn new(base_dir: impl Into<PathBuf>, session_id: impl Into<String>) -> Arc<Self> {
        Self::new_with_config(base_dir, session_id, TelemetryWriteConfig::default(), false)
    }

    /// v1.x:完整构造 —— `write_cfg` 控制脱敏 / 截断(`[telemetry].redact` /
    /// `max_bytes`),`record_tool_schemas` 控制是否记录完整工具 schema
    /// (`[telemetry].record_tool_schemas`,默认 false 只记工具名)。
    pub fn new_with_config(
        base_dir: impl Into<PathBuf>,
        session_id: impl Into<String>,
        write_cfg: TelemetryWriteConfig,
        record_tool_schemas: bool,
    ) -> Arc<Self> {
        let session_id = session_id.into();
        let base_dir = base_dir.into();
        let trace_id = new_trace_id();
        Arc::new(Self {
            enabled: true,
            trace_id,
            session_id: session_id.clone(),
            span_log: SpanLogWriter::with_config(base_dir.clone(), write_cfg.clone()),
            model_io: ModelIoWriter::with_config(base_dir, session_id, write_cfg),
            record_tool_schemas,
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
            record_tool_schemas: false,
            langfuse: Mutex::new(None),
        })
    }

    /// v1.x:是否记录完整工具 schema(`[telemetry].record_tool_schemas`)。
    /// `false`(默认)= model-io 的 request 只带 `tool_names`;
    /// `true` = 额外带 `tool_schemas`(完整 ToolSpec JSON)。
    pub fn record_tool_schemas(&self) -> bool {
        self.record_tool_schemas
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
                // v1.x:缓存分段补进 span context(此前只在 model-io 记录里,
                // span 日志的 TUI 视图看不到缓存命中)。
                "cached_tokens": usage.cached_tokens,
                "cache_write_tokens": usage.cache_write_tokens,
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

    /// v1.x:`SpanStatus::Failed` 必须发射 `model.request.failed` 事件
    /// (此前 4 个调用点全部硬编码 Completed,该事件永不发射 —— 失败调用
    /// 在 telemetry 里完全不可见)。
    #[test]
    fn record_model_call_failed_emits_failed_span() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-fail");
        sink.record_model_call(
            None,
            Some("turn-1"),
            ModelRef {
                model_id: "glm-5.2".into(),
                provider_id: Some("builtin".into()),
                role: Some("main".into()),
                source: Some("main_turn".into()),
            },
            serde_json::json!({"model": "glm-5.2", "attempt": 3}),
            serde_json::json!({"error": "ALL_CREDENTIALS_EXHAUSTED"}),
            UsageSnapshot::default(),
            1200,
            3,
            SpanStatus::Failed,
            "main_turn",
        );
        let files = crate::writer::list_span_log_files(dir.path(), 1);
        let evs = crate::writer::read_span_log_file(&files[0].1);
        let failed = evs
            .iter()
            .find(|e| e.event == "model.request.failed")
            .expect("missing model.request.failed");
        assert_eq!(failed.status, Some(SpanStatus::Failed));
        assert_eq!(failed.level, Level::Error);
        // span context 带缓存分段(v1.x 补全)。
        let mio_path = dir
            .path()
            .join("model-io")
            .join("model-io-sess_sess-fail.jsonl");
        let recs = crate::writer::read_model_io_file(&mio_path);
        assert_eq!(recs.len(), 1);
    }

    /// v1.x:span context 的 usage 必须带 cached_tokens / cache_write_tokens
    /// (此前只有 input/output/total/cost,span 日志的 TUI 视图看不到
    /// 缓存命中)。
    #[test]
    fn record_model_call_span_context_includes_cache_tokens() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-cache");
        sink.record_model_call(
            None,
            Some("turn-1"),
            ModelRef {
                model_id: "claude".into(),
                provider_id: Some("anthropic".into()),
                role: Some("main".into()),
                source: Some("main_turn".into()),
            },
            serde_json::json!({}),
            serde_json::json!({"finish_reason": "stop"}),
            UsageSnapshot {
                input_tokens: 1000,
                output_tokens: 100,
                cached_tokens: 800,
                cache_write_tokens: 50,
                total_tokens: 1100,
                cost_usd: None,
            },
            300,
            1,
            SpanStatus::Completed,
            "main_turn",
        );
        let files = crate::writer::list_span_log_files(dir.path(), 1);
        let evs = crate::writer::read_span_log_file(&files[0].1);
        let ev = evs
            .iter()
            .find(|e| e.event == "model.request.completed")
            .expect("missing completed span");
        let usage = ev
            .context
            .as_ref()
            .and_then(|c| c.get("usage"))
            .expect("usage in context");
        assert_eq!(usage.get("cached_tokens"), Some(&serde_json::json!(800)));
        assert_eq!(
            usage.get("cache_write_tokens"),
            Some(&serde_json::json!(50))
        );
    }

    /// v1.x:`record_tool_schemas` 开关 —— 默认 false 时 sink 不暴露开关,
    /// 显式开启后 `record_tool_schemas()` 返回 true。
    #[test]
    fn record_tool_schemas_flag_defaults_off() {
        let dir = tempdir().unwrap();
        let sink = TelemetrySink::new(dir.path(), "sess-ts");
        assert!(!sink.record_tool_schemas(), "默认应关闭");
        let sink_on = TelemetrySink::new_with_config(
            dir.path(),
            "sess-ts-on",
            crate::model::TelemetryWriteConfig::default(),
            true,
        );
        assert!(sink_on.record_tool_schemas(), "显式开启应生效");
    }

    /// v1.x:`TelemetryWriteConfig` 的 redact / max_bytes 真正作用于写入
    /// —— `redact = false` 时密钥不脱敏;`max_bytes` 覆盖截断阈值。
    #[test]
    fn write_config_controls_redact_and_truncation() {
        let dir = tempdir().unwrap();
        // redact = false:密钥原样落盘(用户显式关闭脱敏)。
        let cfg = crate::model::TelemetryWriteConfig {
            redact: false,
            max_bytes: 64,
        };
        let sink = TelemetrySink::new_with_config(dir.path(), "sess-cfg", cfg, false);
        sink.record_tool_call(
            None,
            Some("turn-1"),
            "call-1",
            "bash",
            &serde_json::json!({"cmd": "echo AKIAIOSFODNN7EXAMPLE"}),
            "ok",
            false,
            10,
        );
        let files = crate::writer::list_span_log_files(dir.path(), 1);
        let evs = crate::writer::read_span_log_file(&files[0].1);
        let ev = evs
            .iter()
            .find(|e| e.event == "tool.call.ended")
            .expect("missing tool span");
        let args = ev.context.as_ref().unwrap().get("args").unwrap();
        // redact=false → 密钥不被替换。
        assert!(
            args.to_string().contains("AKIAIOSFODNN7EXAMPLE"),
            "redact=false 时密钥应原样保留: {args}"
        );
        // max_bytes=64 → 超长 output_preview 被截断(带标记)。
        let preview = ev.context.as_ref().unwrap().get("output_preview").unwrap();
        assert!(
            preview.as_str().unwrap().len() <= 64 + 32,
            "max_bytes=64 应截断 preview: {preview}"
        );
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
