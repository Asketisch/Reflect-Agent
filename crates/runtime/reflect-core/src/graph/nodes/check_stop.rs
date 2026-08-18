//! `check_stop` 节点 — 分发 `Stop` hook。

use reflect_hooks::{HookDecision, HookEvent, StopReason};

use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;

/// `check_stop` — 分发 `Stop` hook。若 hook 拒绝,turn 继续;
/// 否则 turn 结束。
pub async fn check_stop(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    let decision = ctx
        .hook_engine
        .dispatch(&HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: state.stop_hook_attempts,
        })
        .await;
    match decision {
        HookDecision::Deny { reason } => {
            state.stop_hook_attempts = state.stop_hook_attempts.saturating_add(1);
            tracing::warn!(%reason, attempt = state.stop_hook_attempts, "Stop hook vetoed completion");
            Some(GraphNode::PreLoop)
        }
        _ => None,
    }
}
