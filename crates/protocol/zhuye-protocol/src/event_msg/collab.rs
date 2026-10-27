//! M10 / v0.2.4 讨论(collab)生命周期载荷。

use serde::{Deserialize, Serialize};

use super::turn::TokenUsage;

/// M10/v0.2.4: 讨论启动事件。`id` 是 `DiscussionId` UUID 的字符串形式;
/// `participants` 与 `mode` 直接镜像 orchestrator 启动时的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabStartedEvent {
    pub id: String,
    pub participants: Vec<String>,
    pub mode: String,
}

/// M10/v0.2.4: 单条讨论消息事件。每次 `MessageBus::route` 成功投递后
/// orchestrator 会发出一次。`token_usage` 仅在 LLM 路径
///(`collect_result_with_usage` 提供 usage)非空时填入;老 caller 与
/// `run_noop` 路径发出的消息为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabMessageEvent {
    pub id: String,
    pub from: String,
    /// 消息种类序列化形式:`"utterance"` | `"consensus"` | `"finish"`。
    /// 用字符串而非 enum 以避免 `protocol ↔ discussion` 循环依赖。
    pub kind: String,
    pub content: String,
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
}

/// M10/v0.2.4: 讨论结束事件。`outcome` 序列化形式
/// `consensus` | `no_consensus` | `finished`;`rounds` 是实际跑过的轮数
///(可能小于 `max_rounds`)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabFinishedEvent {
    pub id: String,
    pub outcome: String,
    pub rounds: u32,
}
