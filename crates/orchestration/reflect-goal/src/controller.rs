//! `GoalController` —— 每轮 turn 结束后的校验编排核心。
//!
//! 流程(轮次续作机制):
//! 1. 跑 LLM 自校验(+ 可选 verify_command AND 条件)。
//! 2. 更新 `tokens_used` / `turn_count`,check budget → BudgetLimited。
//! 3. 应用 3-strike 规则:verdict=blocked 时 `consecutive_blocked_turns += 1`,
//!    达 BLOCKED_THRESHOLD 才允许 status=Blocked。
//! 4. verdict=met 且命令通过 → status=Complete,退出目标模式。
//! 5. 仍 Active → 生成 continuation prompt,返回让引擎推 steering 续作。
//!
//! controller 持 `Arc<RwLock<Option<GoalState>>>` + verifier LLM client +
//! telemetry sink(写 goal.turn.verified 等事件)。

use std::sync::Arc;

use parking_lot::RwLock;
use reflect_protocol::UserInputItem;
use tokio_util::sync::CancellationToken;

use crate::state::{DEFAULT_TOKEN_BUDGET, GoalState, GoalStatus, GoalTurnRecord, GoalVerdict};
use crate::verifier::{VerifyError, run_verify_command, verify};

/// 引擎在 turn 结束后调用,返回是否需要续作 + 续作 prompt。
pub struct TurnResult {
    /// 续作时的注入文本(None = 不注入,引擎据此判断是否终止)。
    pub continuation_prompt: Option<String>,
    /// 本轮状态变化后的快照(TUI / telemetry 用)。
    pub status: GoalStatus,
    /// 本轮 verdict(met/unmet/blocked)。
    pub verdict: GoalVerdict,
}

/// 目标控制器(一个 session 一个,`NodeContext.goal` 持 `Option<Arc<Self>>`)。
pub struct GoalController {
    state: Arc<RwLock<Option<GoalState>>>,
    /// 校验用 LLM client(可与主 agent 不同)。Arc<dyn> 便于跨 spawn。
    client: Arc<dyn reflect_llm::client::ModelClient>,
    cancel: CancellationToken,
    /// v1.2 P1:可选 telemetry sink,注入后每次 verify 调用落 model-io
    /// 记录(query_source = "goal_verification")。`None`(默认)= 不落库。
    telemetry: Option<Arc<reflect_telemetry::TelemetrySink>>,
}

impl GoalController {
    pub fn new(
        goal: &str,
        verify_command: Option<String>,
        token_budget: Option<u64>,
        client: Arc<dyn reflect_llm::client::ModelClient>,
        cancel: CancellationToken,
        telemetry: Option<Arc<reflect_telemetry::TelemetrySink>>,
    ) -> Self {
        let state = GoalState::new(goal, verify_command, token_budget);
        Self {
            state: Arc::new(RwLock::new(Some(state))),
            client,
            cancel,
            telemetry,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state
            .read()
            .as_ref()
            .is_some_and(|s| s.status == GoalStatus::Active)
    }

    pub fn clear(&self) {
        *self.state.write() = None;
    }

    /// 引擎在 turn 结束后调用(work_context = 本轮 assistant 的最后文本,
    /// tokens_this_turn = 本轮 total_tokens)。
    pub async fn on_turn_end(
        &self,
        work_context: &str,
        tokens_this_turn: u64,
    ) -> Result<TurnResult, VerifyError> {
        // 取出 state 副本(持锁时间短),若已被 clear 返回无续作。
        let mut state = {
            let g = self.state.read();
            match g.as_ref() {
                Some(s) => s.clone(),
                None => {
                    return Ok(TurnResult {
                        continuation_prompt: None,
                        status: GoalStatus::Complete,
                        verdict: GoalVerdict::Met { evidence: vec![] },
                    });
                }
            }
        };
        if state.status != GoalStatus::Active {
            // 非活跃(paused/blocked/etc)→ 不续作。
            return Ok(TurnResult {
                continuation_prompt: None,
                status: state.status,
                verdict: GoalVerdict::Met { evidence: vec![] },
            });
        }

        // 1. LLM 自校验。
        let verdict = verify(
            &state.goal,
            work_context,
            self.client.as_ref(),
            &self.cancel,
            self.telemetry.as_ref(),
        )
        .await?;

        // 2. 可选命令校验(AND 条件:verdict=met 且命令 exit 0 才算真完成)。
        let command_passed = if let Some(cmd) = &state.verify_command {
            if verdict.is_met() {
                Some(run_verify_command(cmd).await)
            } else {
                None
            }
        } else {
            None
        };

        // 3. 更新累计 token + turn count。
        state.turn_count += 1;
        state.tokens_used = state.tokens_used.saturating_add(tokens_this_turn);

        // 4. 状态转移。
        let budget = state.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET);
        let new_status = if state.tokens_used >= budget {
            GoalStatus::BudgetLimited
        } else if verdict.is_met() {
            if command_passed.unwrap_or(true) {
                GoalStatus::Complete
            } else {
                // LLM 判 met 但命令未通过 → 继续尝试,不重置 blocked 计数。
                GoalStatus::Active
            }
        } else if verdict.is_blocked() {
            state.consecutive_blocked_turns += 1;
            if state.should_block() {
                GoalStatus::Blocked
            } else {
                GoalStatus::Active // 继续尝试(3-strike 未达)
            }
        } else {
            // unmet → 重置 blocked 计数,继续工作。
            state.consecutive_blocked_turns = 0;
            GoalStatus::Active
        };

        let should_continue = new_status == GoalStatus::Active;
        let continuation_prompt = if should_continue {
            Some(build_continuation_prompt(&state, &verdict))
        } else {
            None
        };

        // 记历史。
        state.turns.push(GoalTurnRecord {
            turn_index: state.turn_count,
            status_after: new_status,
            verdict: verdict.clone(),
            command_passed,
            tokens_used_this_turn: tokens_this_turn,
            at: chrono::Utc::now(),
        });

        let result = TurnResult {
            continuation_prompt,
            status: new_status,
            verdict: verdict.clone(),
        };

        // 终止态(Complete/Blocked/BudgetLimited)→ clear state(退出目标模式);
        // 否则写回 state。
        if new_status.is_terminal() {
            *self.state.write() = None;
        } else {
            state.status = new_status;
            *self.state.write() = Some(state);
        }

        Ok(result)
    }
}

/// 生成续作 prompt(参考设计规范)。
fn build_continuation_prompt(state: &GoalState, verdict: &GoalVerdict) -> String {
    let mut parts = Vec::new();
    parts.push("Continue working toward the active goal.".to_string());
    parts.push(format!("<objective>{}</objective>", state.goal));
    if let Some(budget) = state.token_budget {
        let remaining = budget.saturating_sub(state.tokens_used);
        parts.push(format!(
            "Token budget: {remaining} tokens remaining (used {}/{budget}).",
            state.tokens_used
        ));
    }
    match verdict {
        GoalVerdict::Met { .. } => {
            // 不应发生(met → 终止),但兜底。
            parts.push("Goal appears complete — do a final check.".into());
        }
        GoalVerdict::Unmet { remaining } => {
            parts.push(format!(
                "Previous verification found these requirements still unmet:\n{}",
                remaining
                    .iter()
                    .map(|r| format!("- {r}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            parts.push(
                "Work from evidence: inspect actual state before relying on prior claims.".into(),
            );
        }
        GoalVerdict::Blocked { reason } => {
            parts.push(format!("Previous attempt blocked: {reason}"));
            parts.push(format!(
                "Blocked attempts: {}/{} before giving up — try a different approach.",
                state.consecutive_blocked_turns,
                crate::state::BLOCKED_THRESHOLD
            ));
        }
    }
    parts.join("\n\n")
}

/// 把 continuation prompt 转成 steering queue 的 items(供引擎推入)。
pub fn continuation_to_items(prompt: &str) -> Vec<UserInputItem> {
    vec![UserInputItem::Text {
        text: format!("[goal continuation] {prompt}"),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_prompt_unmet_lists_remaining() {
        let state = GoalState::new("test goal", None, Some(100_000));
        let verdict = GoalVerdict::Unmet {
            remaining: vec!["fix bug A".into(), "add test B".into()],
        };
        let prompt = build_continuation_prompt(&state, &verdict);
        assert!(prompt.contains("Continue working"));
        assert!(prompt.contains("test goal"));
        assert!(prompt.contains("fix bug A"));
        assert!(prompt.contains("add test B"));
    }

    #[test]
    fn continuation_prompt_blocked_shows_strikes() {
        let mut state = GoalState::new("test", None, None);
        state.consecutive_blocked_turns = 2;
        let verdict = GoalVerdict::Blocked {
            reason: "no DB".into(),
        };
        let prompt = build_continuation_prompt(&state, &verdict);
        assert!(prompt.contains("no DB"));
        assert!(prompt.contains("2/3"));
    }
}
