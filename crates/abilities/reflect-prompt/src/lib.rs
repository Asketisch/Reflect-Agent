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
//! reflect-prompt —— 分层 system prompt 构造与 prompt 缓存支持。
//!
//! Reflect 路线图的 M4。三个模块:
//! - [`template`] —— 轻量 minijinja 封装,做 `{{ var }}` 变量替换。
//! - [`caching`] —— Anthropic `cache_control` 注入 + 变化检测。
//! - [`builder`] —— `LayeredPrompt` 组合(core / append / ephemeral),
//!   以及持有 `CacheBreakDetector`、从分层集合组装 `ChatRequest` 的
//!   `PromptBuilder`。
//!
//! v0 只做 `{{ var }}` 变量替换;不支持 `{% if %}` / `{% for %}` 块
//! (与 Reflect `manager.get_prompt` 语义一致)。

pub mod builder;
pub mod caching;
pub mod resources;
pub mod template;

pub use builder::{
    CORE_PREFIX, EPHEMERAL_PREFIX, LayeredPrompt, MEMORY_HEADER, PLAN_MODE_HEADER, PromptBuilder,
    append_active_mode_section, plan_mode_guidance,
};
pub use caching::{
    CacheBreakDetector, CacheBreakError, CacheMonitorStats, DEFAULT_PREFIX_ANCHOR_OFFSET,
    find_prefix_anchor, inject_cache_control,
};
pub use resources::{PromptResources, copy, prompts};
pub use template::{PromptError, render, render_with};

/// 收敛提示模板:当工具调用应停止、要求模型用此格式给出最终答案时,各收敛
/// 边路(nudge、max-iterations force-final、auto-continue)统一引用此常量,
/// 避免措辞漂移。
///
/// **不常驻** ephemeral `## Important`:运行时没有任何代码消费该标记
/// (turn 收尾判定是「模型不再调工具」),常驻注入只会让照字面执行的模型
/// 把标记回显进每条回答,污染输出。只在迭代预算耗尽等真正需要强收口压力
/// 的边路注入;展示层(TUI/GUI)对历史会话中已存在的标记做渲染剥离。
///
/// 模板值固定为 `"FINAL ANSWER: <answer>"`。现有测试(`single_turn.rs` /
/// `pre_loop_m4.rs`)用 `contains("FINAL ANSWER")` 断言,本常量值变更会
/// 破坏这些断言 —— 改前先 grep 确认。
pub const FINAL_ANSWER_TEMPLATE: &str = "FINAL ANSWER: <answer>";
