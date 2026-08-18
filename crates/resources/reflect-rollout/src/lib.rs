//! `reflect-rollout` —— 带轮转与脱敏的 JSONL rollout 持久化。
//!
//! `reflect-core` 产出的每条 [`reflect_protocol::RolloutRecord`] 都会
//! 追加到 per-thread 文件,位于
//! `~/.reflect/sessions/YYYY/MM/DD/<thread_id>.jsonl`。文件大小到 256 KiB
//! 时触发轮转,最多保留 3 份轮转副本。
//!
//! 大字符串字段截断到 16 KiB 并标记为 `[redacted]`,防止失控的工具输出
//! 把 rollout 撑爆。脱敏 pass 对所有 record 类型统一 —— 不维护
//! key 名的黑名单,因为各 provider 的 schema 差异较大。
//!
//! Resume 只回放历史:[`replay::replay`] 返回该 thread 的全部
//! [`reflect_protocol::RolloutRecord`],由 caller
//! (`reflect-exec::bootstrap_resume`) 丢弃飞行中的工具调用对、
//! 注入一条合成 `<system-reminder>resumed session</system-reminder>`。
//!
//! v1.x:`SessionInfo` 携带 `input_tokens` / `output_tokens` / `total_tokens`
//! / `cost_usd`,从每条 `RolloutRecord::TokenCount` 聚合。旧 jsonl
//! 没有该记录时,四个字段分别为 0 / None。`model_call` 节点在每次 LLM
//! 调用后 best-effort 追加一条 `TokenCount` 记录,让 CLI
//! `reflect session ls/show` 跨进程可见累计。

pub mod export;
pub mod index;
pub mod path;
pub mod reader;
pub mod redact;
pub mod types;
pub mod writer;

pub use export::{MAX_MARKDOWN_CHARS, to_markdown};
pub use reader::{replay, replay_path};
pub use writer::JsonlRolloutWriter;

// `RolloutRecord` 在此 re-export,是因为 `redact.rs` 通过
// `crate::RolloutRecord` 引用它。其它 protocol 类型(MessageRole、
// NullRecorder、RolloutRecorder、SessionInfo)此前也在这里 re-export,
// 但没有使用者 —— caller 直接从 `reflect_protocol` 导入。
pub use reflect_protocol::RolloutRecord;
