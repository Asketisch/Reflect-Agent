//! `check_stop` 节点 — 分发 `Stop` hook。

use reflect_hooks::{HookEvent, StopReason};
use reflect_llm::{ChatMessage, ContentBlock, UserContent};

use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;

/// `check_stop` — 分发 `Stop` hook。若 hook 拒绝,turn 继续;
/// 否则 turn 结束。
///
/// 决策经 [`reflect_hooks::HookDecision::resolve`] 展开:内置 Stop hook
/// (`verification` / `plan_completion`) 否决时返回
/// `Combined([InjectMessage, Deny])`,`HookEngine::merge` 的产物仍是含
/// `Deny` 叶子的 `Combined` —— 此前顶层 `match` 只认单个 `Deny` 变体,
/// 该形态落入 allow 分支,否决被静默丢弃、注入的失败反馈(如测试输出)
/// 也永远到不了模型。现在按叶子判定否决,并把 hook 注入的提醒以
/// system-reminder User 消息追加进历史,让续作轮能看到否决原因。
pub async fn check_stop(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    let decision = ctx
        .hook_engine
        .dispatch(&HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: state.stop_hook_attempts,
        })
        .await;
    let resolved = decision.resolve();
    match resolved.deny_reason {
        Some(reason) => {
            state.stop_hook_attempts = state.stop_hook_attempts.saturating_add(1);
            tracing::warn!(%reason, attempt = state.stop_hook_attempts, "Stop hook vetoed completion");
            // 投递 hook 注入的提醒(否决时才有下一轮 model_call 可注入)。
            for m in &resolved.injected {
                state.messages.messages.push(ChatMessage::User(UserContent {
                    blocks: vec![ContentBlock::Text {
                        text: format!("<system-reminder>\n{}\n</system-reminder>", m.content),
                    }],
                }));
            }
            Some(GraphNode::PreLoop)
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_hooks::{HookDecision, HookEngine, SystemMessage};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// 仅用于 `check_stop` 的最小 `NodeContext` —— 只填充 hook_engine,
    /// 其余字段取空值(check_stop 只读 `hook_engine` / 写 `state`)。
    fn minimal_ctx(engine: Arc<HookEngine>) -> NodeContext {
        let (event_tx, _rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(8);
        NodeContext {
            turn_id: reflect_protocol::TurnId::new(),
            session_id: reflect_protocol::ThreadId::new(),
            model: Arc::new(parking_lot::RwLock::new("stub/model".to_string())),
            policy: Arc::new(reflect_llm::RoutingPolicy::default()),
            registry: Arc::new(reflect_llm::ModelRegistry::new()),
            hook_engine: engine,
            tools_queue: Arc::new(reflect_tools::ToolExecutionQueue::with_defaults(
                Arc::new(reflect_tools::ToolRegistry::new()),
                Arc::new(HookEngine::new()),
                reflect_tools::ToolContext::default(),
            )),
            sub_id: "sub-test".into(),
            cancel: CancellationToken::new(),
            event_tx,
            messages: vec![],
            max_iterations: 32,
            m4: None,
            recorder: None,
            approval_gate: None,
            effort: Arc::new(parking_lot::RwLock::new(
                reflect_protocol::ReasoningEffortMirror::Medium,
            )),
            session_usage: Arc::new(parking_lot::RwLock::new(
                reflect_protocol::TokenUsage::default(),
            )),
            token_budget: Arc::new(parking_lot::RwLock::new(None)),
            force_compact_next: Arc::new(parking_lot::RwLock::new(false)),
            telemetry: None,
            goal: None,
            quota_tracker: None,
            plan_approval_gate: None,
            plan_session_subs: None,
            cfg: crate::config::AgentConfig::new("stub/model", "/tmp"),
        }
    }

    /// 注册一个返回 `Combined([InjectMessage, Deny])` 的 Stop hook ——
    /// 与内置 `verification` / `plan_completion` 否决时的形态一致。
    struct CombinedDenyStopHook;
    #[async_trait::async_trait]
    impl reflect_hooks::Hook for CombinedDenyStopHook {
        fn name(&self) -> &str {
            "combined-deny-stop"
        }
        fn events(&self) -> &[reflect_hooks::HookEventKind] {
            &[reflect_hooks::HookEventKind::Stop]
        }
        async fn handle(
            &self,
            _event: &HookEvent,
        ) -> Result<HookDecision, reflect_hooks::HookError> {
            Ok(HookDecision::Combined(vec![
                HookDecision::InjectMessage(SystemMessage::new("tests failing: 2 failed")),
                HookDecision::Deny {
                    reason: "tests failing".into(),
                },
            ]))
        }
    }

    /// 回归:Stop hook 以 `Combined([InjectMessage, Deny])` 否决时,
    /// `check_stop` 必须返回 `PreLoop`(续作),而不是 allow 终止;
    /// 且注入的提醒必须进历史,供下一轮 model_call 看见。
    #[tokio::test]
    async fn combined_deny_vetoes_and_injects_message() {
        let engine = Arc::new(HookEngine::new());
        engine.register(CombinedDenyStopHook);
        let ctx = minimal_ctx(engine);
        let mut state = AgentState::default();

        let next = check_stop(&mut state, &ctx).await;
        assert_eq!(
            next,
            Some(GraphNode::PreLoop),
            "Combined([InjectMessage, Deny]) 必须否决 turn 完成"
        );
        assert_eq!(state.stop_hook_attempts, 1);
        // 注入的提醒必须以 system-reminder 形态追加进历史。
        assert_eq!(state.messages.messages.len(), 1);
        match &state.messages.messages[0] {
            ChatMessage::User(u) => {
                assert!(u.blocks.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains("tests failing"))
                ));
            }
            other => panic!("expected User reminder message, got {other:?}"),
        }
    }

    /// 无 hook 时 allow → turn 自然结束。
    #[tokio::test]
    async fn no_hooks_terminates() {
        let ctx = minimal_ctx(Arc::new(HookEngine::new()));
        let mut state = AgentState::default();
        assert_eq!(check_stop(&mut state, &ctx).await, None);
        assert_eq!(state.stop_hook_attempts, 0);
    }
}
