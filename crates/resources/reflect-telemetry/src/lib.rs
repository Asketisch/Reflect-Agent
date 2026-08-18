//! Reflect Telemetry —— 本地 Langfuse 式日志(TUI 可查看)。
//!
//! 两层 JSONL 存储,对齐 zcode 的本地日志结构(不含 SQLite):
//! - **span 日志** `<base>/log/reflect-YYYY-MM-DD.jsonl` —— 按天 + size 轮转,
//!   每行一个 [`model::TraceEvent`](turn/model/tool 级 span),带 trace/span
//!   父子链。相当于 Langfuse 的 trace 事件流。
//! - **model-io** `<base>/model-io/model-io-sess_<id>.jsonl` —— 按 session 一个
//!   文件,每行一个 [`model::ModelIoRecord`](完整 LLM 请求/响应/usage/cost/
//!   latency)。相当于 Langfuse 的 `generation` 记录。
//!
//! 引擎通过 [`sink::TelemetrySink`] 高级 API 写入;TUI 通过
//! [`writer::list_span_log_files`] / [`writer::read_span_log_file`] /
//! [`writer::read_model_io_file`] 读取并在 `/traces` overlay 展示。
//!
//! 设计取舍:
//! - 不依赖 `tracing` crate 的 span(因 TUI 根本没装 subscriber,span 全丢)。
//!   直接订阅 `EventMsg` 流 + 显式时间戳,100% 可靠。
//! - 写失败只 `tracing::warn!`,绝不中断在途 turn(best-effort)。
//! - 16 KiB 字段截断脱敏(复用 rollout `redact_value` 的同一套思路)。
//! - 覆盖本地模型:Ollama 已返回真实 token(`prompt_eval_count`/`eval_count`),
//!   引擎补读 `total_duration`/`eval_duration` 填 latency。

pub mod langfuse;
pub mod model;
pub mod sink;
pub mod writer;

pub use langfuse::{LangfuseConfig, LangfuseExporter};
pub use model::{
    DEFAULT_TRACES_DIR, Level, ModelIoRecord, ModelRef, SpanStatus, TraceEvent, UsageSnapshot,
};
pub use sink::{TelemetrySink, TurnSpan, resolve_traces_dir};
pub use writer::{
    ModelIoWriter, SpanLogWriter, list_model_io_files, list_span_log_files, read_model_io_file,
    read_span_log_file,
};
