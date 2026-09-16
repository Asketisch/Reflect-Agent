#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! reflect-compact —— 上下文压缩策略。
//!
//! Reflect 路线图的 M4 阶段。对应 reflect 的 `graph.py:330-609`
//! (microcompact + smart_prune)与 `summarizer.py`(LLM-summarize)
//! 的移植版。
//!
//! 三种压缩策略,外加一个 noop:
//!
//! - [`microcompact`] —— 本地启发式,不调用 LLM。固定保留 `System` 与
//!   第一条 `User`;将旧工具结果替换为占位符;剥离 thinking 块。
//! - [`smart_prune`] —— 本地启发式,不调用 LLM。按工具类型应用截断
//!   规则(grep → 头部,bash → 尾部,read → 头尾,其余 → 按字符)。
//!   在低于 `target_tokens` 之前,持续丢弃最旧的非固定保留消息。
//! - [`summarizer::Summarizer`] + [`LlmSummarizer`] —— 异步,LLM 驱动。
//!   生成包含 9 个章节的中文摘要,以 `<summary>` 标签包裹。
//!
//! [`strategy::Compactor`] 将上述三者串联:估算 token,先执行
//! microcompact,再升级到 smart_prune,最后调用 LLM 摘要。

pub mod microcompact;
pub mod smart_prune;
pub mod strategy;
pub mod summarizer;
pub mod tokens;
pub mod tool_pair;

pub use microcompact::{
    CompactReport, KEEP_RECENT_DEFAULT, MICROCOMPACT_TRIGGER_RATIO, MicrocompactConfig,
    PRESERVE_TOOL_NAMES, TRUNCATABLE_TOOL_NAMES, microcompact,
};
pub use smart_prune::{
    MAX_ASSISTANT_CHARS, MAX_TOOL_RESULT_CHARS, MAX_TOOL_RESULT_LINES, SmartPruneConfig,
    smart_prune,
};
pub use strategy::{Compactor, CompactorConfig, DEFAULT_TRIGGER_TOKENS};
pub use summarizer::{
    LlmSummarizer, SUMMARIZE_PROMPT_FULL, SUMMARIZE_PROMPT_RECENT, SUMMARIZE_TIMEOUT, Summarizer,
    SummarizerError,
};
#[cfg(feature = "tokenizer")]
pub use tokens::tiktoken::{TiktokenEstimator, global_tiktoken_estimator};
pub use tokens::{
    HeuristicEstimator, TokenEstimator, estimate_messages, estimate_text, global_estimator,
    set_global_estimator,
};
