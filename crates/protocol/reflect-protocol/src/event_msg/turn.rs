//! Turn 生命周期载荷 + token 用量 / 状态子类型。

use serde::{Deserialize, Serialize};

use crate::item::TurnId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleteEvent {
    pub turn_id: TurnId,
    pub usage: TokenUsage,
    pub status: TurnStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnAbortedEvent {
    pub turn_id: TurnId,
    pub reason: AbortReason,
}

/// 批次十九:`Op::Rewind` 成功后发出,让 TUI 裁剪显示到回退点。
/// `truncated_after` = 被丢弃的 turn 数(0 = 已是最近一条,无可回退)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRewoundEvent {
    /// 回退到的 turn(`None` = 回退到 session 起 / 最近 user turn)。
    pub to_turn_id: Option<String>,
    /// 被丢弃的 turn 数(供 TUI Pill 文案)。
    pub truncated_after: usize,
}

/// Token 用量快照。镜像 `reflect-llm` 的 `ChatEvent::Usage`。
///
/// **Token 计费口径(M8)**:`input_tokens` 是 LLM 上报的原始 `input`
/// 字段(Anthropic 的 `input_tokens` 已折入 `cache_creation_input_tokens`
/// 这段子片段)。`cached_tokens` 是 `cache_read_input_tokens` 折扣子片段,
/// 是 `input_tokens` 的一个**子集**,而非累加。`cache_write_tokens` 是
/// `cache_creation_input_tokens` 子片段,同样是 `input_tokens` 的子集。
/// 计费请使用 `pricing::price(model, usage)`,它会把四段按各自的倍率
/// 分摊。
///
/// `total_tokens = input_tokens + output_tokens` 对应线上可计费数量,
/// **故意保持与 M7 一致**;不要把 `cache_write_tokens` 或 `cached_tokens`
/// 加进去(否则会重复计费)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_tokens: u32,
    /// M8:缓存写入分段(cache_creation)。已折入 `input_tokens`;**不要**
    /// 再计入 `total_tokens`。`#[serde(default)]` 保证与省略该字段的
    /// M7 消费者保持 wire 兼容。
    #[serde(default)]
    pub cache_write_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    pub fn new(input: u32, output: u32, cached: u32) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: 0,
            total_tokens: input + output,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Success,
    MaxIterations,
    Stopped,
    /// v1.2 P1-12:会话级 token 预算耗尽(`[token_budget].session_total_tokens`
    /// 或 env `REFLECT_TOKEN_BUDGET` 设的上限)后终止当前 turn。与
    /// `MaxIterations`(步数上限)区分,便于 TUI / 日志归因。
    TokenBudgetExceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AbortReason {
    UserInterrupt,
    Error { code: String, message: String },
    Shutdown,
}
