//! Goal 状态机 —— 参考 `thread_goals` 表设计 + zcode 的 LLM 自校验。
//!
//! 状态枚举 `GoalStatus` 镜像状态机约束设计:
//! `active / paused / blocked / budget_limited / complete`。
//!
//! 关键安全机制:
//! - **3-strike blocked 规则**:同一 blocker 连续重复 ≥ 3 个 goal turn 才
//!   允许标 `blocked`(防 LLM 遇到小障碍就放弃)。
//! - **token budget**:累计达上限 → `budget_limited`(软停,让 agent 收尾)。
//! - **无硬 turn 上限**:靠 budget + 3-strike + 用户 `/goal pause` 兜底。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 目标状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// 活跃,agent 正在朝目标工作。
    Active,
    /// 用户暂停(`/goal pause`)。不自动续作。
    Paused,
    /// 同一 blocker 连续 ≥ 3 个 goal turn → 标 blocked(防无限循环)。
    Blocked,
    /// token 预算耗尽 → 软停,让 agent 收尾 + 提示用户。
    BudgetLimited,
    /// 目标已验证完成(退出目标模式)。
    Complete,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::BudgetLimited => "budget_limited",
            GoalStatus::Complete => "complete",
        }
    }

    /// 是否是终止态(不再自动续作)。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            GoalStatus::Complete | GoalStatus::Blocked | GoalStatus::BudgetLimited
        )
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// LLM 自校验的判定结果(对齐 zcode `target_completion_verification`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum GoalVerdict {
    /// 目标已达成 —— 每条需求都有当前证据支持。
    Met {
        /// 支持完成的证据摘要(每条需求一行)。
        evidence: Vec<String>,
    },
    /// 目标尚未达成 —— 有未完成的需求。
    Unmet {
        /// 仍需完成的需求列表。
        remaining: Vec<String>,
    },
    /// 遇到外部阻塞(无权限 / 缺依赖 / 环境不可用)。
    Blocked {
        /// 阻塞原因。
        reason: String,
    },
}

impl GoalVerdict {
    pub fn is_met(&self) -> bool {
        matches!(self, GoalVerdict::Met { .. })
    }
    pub fn is_blocked(&self) -> bool {
        matches!(self, GoalVerdict::Blocked { .. })
    }
}

/// 单轮 goal 校验记录(存历史,供 TUI 渲染 + telemetry)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTurnRecord {
    pub turn_index: u32,
    pub status_after: GoalStatus,
    /// LLM 判定(met/unmet/blocked + 证据/剩余/原因)。
    pub verdict: GoalVerdict,
    /// 可选命令校验结果(若有 verify_command)。
    pub command_passed: Option<bool>,
    pub tokens_used_this_turn: u64,
    pub at: DateTime<Utc>,
}

/// 目标运行时状态(controller 持有,`Arc<RwLock<Option<GoalState>>>`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub goal: String,
    pub verify_command: Option<String>,
    pub token_budget: Option<u64>,
    pub status: GoalStatus,
    /// 累计 token(跨所有 goal turn)。
    pub tokens_used: u64,
    /// 累计 goal turn 数(每次 on_turn_end +1)。
    pub turn_count: u32,
    /// 连续 blocked 计数(3-strike 规则)。
    pub consecutive_blocked_turns: u32,
    pub started_at: DateTime<Utc>,
    /// 每轮校验历史(供 TUI + telemetry)。
    pub turns: Vec<GoalTurnRecord>,
}

impl GoalState {
    pub fn new(
        goal: impl Into<String>,
        verify_command: Option<String>,
        token_budget: Option<u64>,
    ) -> Self {
        Self {
            goal: goal.into(),
            verify_command,
            token_budget,
            status: GoalStatus::Active,
            tokens_used: 0,
            turn_count: 0,
            consecutive_blocked_turns: 0,
            started_at: Utc::now(),
            turns: Vec::new(),
        }
    }

    /// 3-strike:连续 blocked 达 BLOCKED_THRESHOLD 才允许标 Blocked。
    pub fn should_block(&self) -> bool {
        self.consecutive_blocked_turns >= BLOCKED_THRESHOLD
    }
}

/// 连续 blocked turn 阈值(3 次规则)。
pub const BLOCKED_THRESHOLD: u32 = 3;

/// 默认 token 预算(None 时的兜底,防无限循环)。
pub const DEFAULT_TOKEN_BUDGET: u64 = 500_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_terminal_check() {
        assert!(GoalStatus::Complete.is_terminal());
        assert!(GoalStatus::Blocked.is_terminal());
        assert!(GoalStatus::BudgetLimited.is_terminal());
        assert!(!GoalStatus::Active.is_terminal());
        assert!(!GoalStatus::Paused.is_terminal());
    }

    #[test]
    fn verdict_helpers() {
        assert!(GoalVerdict::Met { evidence: vec![] }.is_met());
        assert!(!GoalVerdict::Unmet { remaining: vec![] }.is_met());
        assert!(GoalVerdict::Blocked { reason: "x".into() }.is_blocked());
    }

    #[test]
    fn new_state_is_active() {
        let s = GoalState::new("test", None, None);
        assert_eq!(s.status, GoalStatus::Active);
        assert_eq!(s.turn_count, 0);
        assert_eq!(s.consecutive_blocked_turns, 0);
    }

    #[test]
    fn should_block_only_after_threshold() {
        let mut s = GoalState::new("test", None, None);
        assert!(!s.should_block());
        s.consecutive_blocked_turns = 2;
        assert!(!s.should_block());
        s.consecutive_blocked_turns = 3;
        assert!(s.should_block());
    }
}
