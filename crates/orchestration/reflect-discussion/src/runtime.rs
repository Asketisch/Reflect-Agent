//! `runtime` — 讨论执行循环(顺序 / 并发两种模式)。
//!
//! 不直接管理 LLM:把每轮的"收集 mailbox snapshot → 构造 prompt → 调 LLM"
//! 委托给 caller 提供的 [`FnMut(AgentId, u32, Vec<DiscussionMessage>) -> F`]
//! 闭包。这样 `DiscussionRuntime` 与 `reflect-llm` 解耦,方便单测用 stub 闭包。
//!
//! 顺序模式:每轮一个 agent 跑完(闭包 await)再下一个。
//! 并发模式:每轮所有 agent 通过 [`tokio::task::JoinSet`] 并行 spawn,收齐
//! 后再开下一轮。
//!
//! 共识检测:每轮结束后扫 transcript,在最近 `consensus_window` 轮内所有
//! participant 都发过 [`MessageKind::Consensus`] 时整组达成共识。
//!
//! 主动结束:若 transcript 出现 [`MessageKind::Finish`] 消息(由
//! [`FinishDiscussionTool`](crate::comm_tools::FinishDiscussionTool) 触发),
//! 整组立即退出并返回 [`DiscussionResult::Finished`]。

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use thiserror::Error;
use tokio::task::JoinSet;
use tracing::{debug, info};

use crate::message_bus::MessageBus;
use crate::models::{AgentId, DiscussionConfig, DiscussionMessage, DiscussionResult, MessageKind};
use crate::orchestrator::OrchestratorEvent;

/// 调度循环错误。
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// 调用方主动取消(由 [`tokio_util::sync::CancellationToken`] 触发)。
    #[error("cancelled")]
    Cancelled,
    /// `prompt_for` 闭包返回 `Err` —— 把字符串透传给调用方。
    #[error("prompt builder failed: {0}")]
    PromptBuilder(String),
}

/// 顺序/并发调度循环的共享状态。
///
/// `MessageBus` 自身实现 `Clone`(内部 `Arc<Inner>`),`DiscussionRuntime`
/// 直接持有 `MessageBus`;`&MessageBus` 跨 `await` 借用是 `Send`,所以
/// async 方法签名不需要 `Mutex` 包装。
///
/// v0.2.3 起:`round_counter` 是 `Arc<AtomicU32>`,由 `run_sequential` /
/// `run_concurrent` 在每轮开始前 `store(round, SeqCst)`;同时这个 `Arc`
/// 会被 `DiscussionToolSet` 共享,comm_tools 在 `execute()` 时 `load` 后
/// 写入 `DiscussionMessage.round`,保证 transcript 里的消息带正确的轮次
/// 标记(`consensus_window > 1` 的跨轮共识检测依赖于此)。
#[derive(Debug, Clone)]
pub struct DiscussionRuntime {
    pub config: DiscussionConfig,
    pub bus: MessageBus,
    /// 共享轮次计数器(v0.2.3+);同 `DiscussionToolSet::round` 必须为同一
    /// `Arc<AtomicU32>` 实例,否则 comm_tools 读不到 runtime 写入的轮次。
    pub round_counter: Arc<AtomicU32>,
}

impl DiscussionRuntime {
    /// 构造一个 runtime,绑定一组配置、共享 bus 和共享轮次计数器。
    ///
    /// 默认轮次计数器为 `0`;CLI / 程序化构造者通常会从外部传入同一个
    /// `Arc<AtomicU32>` 给 runtime + 多个 `DiscussionToolSet` + orchestrator。
    pub fn new(config: DiscussionConfig, bus: MessageBus, round_counter: Arc<AtomicU32>) -> Self {
        Self {
            config,
            bus,
            round_counter,
        }
    }

    /// 兼容构造:起一个全新的 `Arc<AtomicU32>` 作为轮次计数器(适合单测、
    /// `run_noop` 等不需要 comm_tools 写 round 的场景)。
    pub fn with_default_round(config: DiscussionConfig, bus: MessageBus) -> Self {
        Self::new(config, bus, Arc::new(AtomicU32::new(0)))
    }

    /// 顺序模式:每轮按 `participants` 顺序逐个执行,所有 agent 跑完算一轮结束。
    ///
    /// v0.2.3 起:每次 `prompt_for` 返回 `Ok(())` 后立即调用
    /// `on_event(OrchestratorEvent::AgentTurn { agent, round })`,让 caller
    /// 能在 CLI log / TUI 看到每个 agent 的推进。`OE` 与 `P` / `F` 是独立
    /// 泛型参数,允许 caller 用不同的 closure 类型(比如 `&mut` 借用一个
    /// `Vec<OrchestratorEvent>` 而不是 boxed `FnMut`)。
    pub async fn run_sequential<P, F, OE>(
        &self,
        mut prompt_for: P,
        mut on_event: OE,
    ) -> Result<DiscussionResult, RuntimeError>
    where
        P: FnMut(AgentId, u32, Vec<DiscussionMessage>) -> F,
        F: Future<Output = Result<(), RuntimeError>>,
        OE: FnMut(OrchestratorEvent),
    {
        let mut round: u32 = 0;
        let mut last_consensus = String::new();
        let mut finished_by: Option<AgentId> = None;

        'outer: while round < self.config.max_rounds {
            // v0.2.3 起:把当前轮次写入共享计数器,comm_tools 在 execute
            // 时读取后写入 `DiscussionMessage.round`,保证 transcript 里
            // 消息的 round 字段反映真实轮次(consensus_window > 1 时
            // 跨轮共识检测依赖此标记)。
            self.round_counter.store(round, Ordering::SeqCst);
            info!(round, mode = "sequential", "discussion round start");
            for agent in self.config.participants.clone() {
                let snapshot = self.collect_snapshot(&agent);
                prompt_for(agent.clone(), round, snapshot).await?;
                on_event(OrchestratorEvent::AgentTurn { agent, round });
                if self.check_finished(&mut finished_by) {
                    debug!("finish signal seen, exiting outer loop");
                    break 'outer;
                }
            }
            if self.check_consensus(round, &mut last_consensus) {
                return Ok(DiscussionResult::Consensus {
                    final_round: round,
                    summary: last_consensus.clone(),
                });
            }
            round += 1;
        }

        Ok(self.finalize(round, finished_by))
    }

    /// 并发模式:每轮所有 agent 通过 [`JoinSet`] 并行 spawn,收齐后再开下一轮。
    ///
    /// 闭包 trait bound 要求 `Send + 'static + Clone`,因为每个 task 都需要一份
    /// 闭包副本;闭包 capture 的状态必须是 `Send`(典型是 `Arc<...>` 或
    /// `MessageBus` 本身)。
    ///
    /// v0.2.3 起:每次 `prompt_for` 返回 `Ok(())` 后立即调用
    /// `on_event(OrchestratorEvent::AgentTurn { agent, round })`(`Arc` 共享
    /// capture 满足 `Send + 'static`)。
    pub async fn run_concurrent<P, F, OE>(
        &self,
        prompt_for: P,
        mut on_event: OE,
    ) -> Result<DiscussionResult, RuntimeError>
    where
        P: FnMut(AgentId, u32, Vec<DiscussionMessage>) -> F + Send + 'static + Clone,
        F: Future<Output = Result<(), RuntimeError>> + Send + 'static,
        OE: FnMut(OrchestratorEvent) + Send + 'static,
    {
        let mut round: u32 = 0;
        let mut last_consensus = String::new();
        let mut finished_by: Option<AgentId> = None;

        'outer: while round < self.config.max_rounds {
            // v0.2.3 起:并发模式下同样在每轮开始前 store 当前轮次。
            self.round_counter.store(round, Ordering::SeqCst);
            info!(round, mode = "concurrent", "discussion round start");
            // on_event 在并发模式下需要 'static + Send,所以包到 Arc<Mutex<_>>。
            // 闭包类型实现为 `FnMut(OrchestratorEvent) + Send + 'static`
            // —— 我们用 `parking_lot::Mutex<Vec<OrchestratorEvent>>` 包装,
            // 每次 emit 拿锁 push,避免 self-referential closure 复杂度。
            let events_slot: Arc<parking_lot::Mutex<Vec<OrchestratorEvent>>> =
                Arc::new(parking_lot::Mutex::new(Vec::new()));
            let mut set: JoinSet<Result<(), RuntimeError>> = JoinSet::new();
            for agent in self.config.participants.clone() {
                let snapshot = self.collect_snapshot(&agent);
                let mut p = prompt_for.clone();
                let events_slot = events_slot.clone();
                set.spawn(async move {
                    p(agent.clone(), round, snapshot).await?;
                    events_slot
                        .lock()
                        .push(OrchestratorEvent::AgentTurn { agent, round });
                    Ok(())
                });
            }
            // 排空本轮事件;若触发 finish 信号则中止剩余任务。
            let mut saw_finish = false;
            while let Some(res) = set.join_next().await {
                res.map_err(|_| RuntimeError::Cancelled)??;
                if self.check_finished(&mut finished_by) {
                    saw_finish = true;
                    break;
                }
            }
            // 把本轮累积的事件排空到 caller 的 on_event(顺序保持 push 顺序)。
            for ev in events_slot.lock().drain(..) {
                on_event(ev);
            }
            if saw_finish {
                set.abort_all();
                while set.join_next().await.is_some() {}
                break 'outer;
            }
            if self.check_consensus(round, &mut last_consensus) {
                return Ok(DiscussionResult::Consensus {
                    final_round: round,
                    summary: last_consensus.clone(),
                });
            }
            round += 1;
        }

        Ok(self.finalize(round, finished_by))
    }

    /// 收集某个 agent 视角下可见的 transcript:
    /// - 广播消息(`recipients` 为空)对所有 agent 可见
    /// - 单播消息只对 `recipients` 列表中的 agent 可见
    fn collect_snapshot(&self, agent: &AgentId) -> Vec<DiscussionMessage> {
        self.bus
            .transcript()
            .into_iter()
            .filter(|m| m.recipients.is_empty() || m.recipients.contains(agent))
            .collect()
    }

    /// 共识检测:扫 transcript,统计"在最近 `consensus_window` 轮内,所有
    /// participant 是否都至少发过一条 `Consensus` 消息"。
    ///
    /// `consensus_window = 0` 时关闭窗口(永不达成共识,等 `max_rounds` 兜底)。
    /// `consensus_window = 1`(默认)表示"当前轮所有人都发 Consensus 即达成"。
    fn check_consensus(&self, current_round: u32, last_summary: &mut String) -> bool {
        if self.config.consensus_window == 0 {
            return false;
        }
        let bus_transcript = self.bus.transcript();
        let window = self.config.consensus_window;
        let window_start = current_round.saturating_sub(window - 1);
        let mut seen: HashSet<&AgentId> = HashSet::new();
        let mut last_consensus_msg: Option<&DiscussionMessage> = None;
        for m in bus_transcript.iter().rev() {
            if m.round < window_start {
                break;
            }
            if matches!(m.kind, MessageKind::Consensus) && seen.insert(&m.from) {
                if last_consensus_msg.is_none() {
                    last_consensus_msg = Some(m);
                }
            }
        }
        let all_reached = self.config.participants.iter().all(|p| seen.contains(p));
        if all_reached {
            if let Some(m) = last_consensus_msg {
                *last_summary = m.content.clone();
            }
        }
        all_reached
    }

    /// 主动结束检测:transcript 出现任何 `Finish` 消息时,记录触发者。
    /// 同一讨论中多次 Finish 只记**第一次**的触发者。
    fn check_finished(&self, finished_by: &mut Option<AgentId>) -> bool {
        if finished_by.is_some() {
            return true;
        }
        if let Some(msg) = self
            .bus
            .transcript()
            .iter()
            .rev()
            .find(|m| matches!(m.kind, MessageKind::Finish))
        {
            *finished_by = Some(msg.from.clone());
            return true;
        }
        false
    }

    /// 退出 outer loop 后构造 [`DiscussionResult`]:Finish 优先,否则 NoConsensus。
    fn finalize(&self, rounds_completed: u32, finished_by: Option<AgentId>) -> DiscussionResult {
        match finished_by {
            Some(by) => DiscussionResult::Finished {
                by,
                final_round: rounds_completed.saturating_sub(1),
            },
            None => DiscussionResult::NoConsensus {
                rounds_completed,
                transcript_len: self.bus.transcript_len(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use crate::models::{
        DiscussionConfig, DiscussionId, DiscussionMessage, DiscussionMode, MessageKind,
    };

    /// Helper:构造一个返回 `Ok(())` 且什么都不做的 prompt_for(只 trigger 一
    /// 个空转 round);后面测试在闭包里塞具体行为。
    fn noop_prompt_for(
        _agent: AgentId,
        _round: u32,
        _snapshot: Vec<DiscussionMessage>,
    ) -> std::future::Ready<Result<(), RuntimeError>> {
        std::future::ready(Ok(()))
    }

    /// Helper:on_event 空实现(不收集任何事件);让 run_sequential /
    /// run_concurrent 的第二个参数有具体类型。
    fn noop_on_event(_event: OrchestratorEvent) {}

    fn mk_config(
        mode: DiscussionMode,
        max_rounds: u32,
        consensus_window: u32,
    ) -> (DiscussionConfig, MessageBus) {
        let cfg = DiscussionConfig {
            mode,
            participants: vec![
                AgentId("a".into()),
                AgentId("b".into()),
                AgentId("c".into()),
            ],
            topic: "x".into(),
            consensus_window,
            max_rounds,
            mailbox_capacity: 8,
        };
        let bus = MessageBus::new(
            DiscussionId::new(),
            cfg.participants.clone(),
            cfg.mailbox_capacity,
        );
        (cfg, bus)
    }

    // ── 顺序模式执行(run_sequential)───────────────────────

    #[tokio::test]
    async fn sequential_no_messages_returns_no_consensus() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 2, 1);
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(noop_prompt_for, noop_on_event)
            .await
            .unwrap();
        assert!(
            matches!(
                r,
                DiscussionResult::NoConsensus {
                    rounds_completed: 2,
                    ..
                }
            ),
            "got: {r:?}"
        );
    }

    #[tokio::test]
    async fn sequential_all_agents_consensus_reached() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 3, 1);
        // 注入 3 条 consensus(round=0)
        {
            let b = bus.clone();
            for a in &["a", "b", "c"] {
                b.route(DiscussionMessage {
                    id: Default::default(),
                    discussion_id: b.discussion_id(),
                    from: AgentId((*a).into()),
                    kind: MessageKind::Consensus,
                    content: format!("{a} agrees"),
                    recipients: vec![],
                    round: 0,
                    token_usage: Default::default(),
                })
                .await
                .unwrap();
            }
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(noop_prompt_for, noop_on_event)
            .await
            .unwrap();
        assert!(
            matches!(r, DiscussionResult::Consensus { final_round: 0, .. }),
            "got: {r:?}"
        );
    }

    #[tokio::test]
    async fn sequential_partial_consensus_does_not_reach() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 2, 1);
        // 只 a 和 b 发 consensus,c 没发
        {
            let b = bus.clone();
            for a in &["a", "b"] {
                b.route(DiscussionMessage {
                    id: Default::default(),
                    discussion_id: b.discussion_id(),
                    from: AgentId((*a).into()),
                    kind: MessageKind::Consensus,
                    content: "agree".into(),
                    recipients: vec![],
                    round: 0,
                    token_usage: Default::default(),
                })
                .await
                .unwrap();
            }
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(noop_prompt_for, noop_on_event)
            .await
            .unwrap();
        assert!(
            matches!(r, DiscussionResult::NoConsensus { .. }),
            "got: {r:?}"
        );
    }

    #[tokio::test]
    async fn sequential_finish_signal_exits_early() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 5, 1);
        // moderator 在 round 0 调 finish_discussion
        {
            let b = bus.clone();
            b.route(DiscussionMessage {
                id: Default::default(),
                discussion_id: b.discussion_id(),
                from: AgentId("b".into()),
                kind: MessageKind::Finish,
                content: "we're done".into(),
                recipients: vec![],
                round: 0,
                token_usage: Default::default(),
            })
            .await
            .unwrap();
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(noop_prompt_for, noop_on_event)
            .await
            .unwrap();
        match r {
            DiscussionResult::Finished { by, final_round } => {
                assert_eq!(by, AgentId("b".into()));
                assert_eq!(final_round, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sequential_consensus_window_zero_never_consensus() {
        // consensus_window=0 关闭窗口;3 agent 全部发了 Consensus 也不达成
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 1, 0);
        {
            let b = bus.clone();
            for a in &["a", "b", "c"] {
                b.route(DiscussionMessage {
                    id: Default::default(),
                    discussion_id: b.discussion_id(),
                    from: AgentId((*a).into()),
                    kind: MessageKind::Consensus,
                    content: "x".into(),
                    recipients: vec![],
                    round: 0,
                    token_usage: Default::default(),
                })
                .await
                .unwrap();
            }
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(noop_prompt_for, noop_on_event)
            .await
            .unwrap();
        assert!(
            matches!(r, DiscussionResult::NoConsensus { .. }),
            "got: {r:?}"
        );
    }

    // ── 并发模式执行(run_concurrent)───────────────────────

    #[tokio::test]
    async fn concurrent_all_agents_consensus_reached() {
        let (cfg, bus) = mk_config(DiscussionMode::Concurrent, 3, 1);
        {
            let b = bus.clone();
            for a in &["a", "b", "c"] {
                b.route(DiscussionMessage {
                    id: Default::default(),
                    discussion_id: b.discussion_id(),
                    from: AgentId((*a).into()),
                    kind: MessageKind::Consensus,
                    content: "agree".into(),
                    recipients: vec![],
                    round: 0,
                    token_usage: Default::default(),
                })
                .await
                .unwrap();
            }
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        // concurrent prompt 必须是 Send + 'static + Clone
        let r = rt
            .run_concurrent(
                |_a, _r, _s| async { Ok(()) },
                noop_on_event as fn(OrchestratorEvent),
            )
            .await
            .unwrap();
        assert!(
            matches!(r, DiscussionResult::Consensus { final_round: 0, .. }),
            "got: {r:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_max_rounds_forces_no_consensus() {
        let (cfg, bus) = mk_config(DiscussionMode::Concurrent, 3, 1);
        // 不注入任何消息;跑满 3 轮
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_concurrent(
                |_a, _r, _s| async { Ok(()) },
                noop_on_event as fn(OrchestratorEvent),
            )
            .await
            .unwrap();
        match r {
            DiscussionResult::NoConsensus {
                rounds_completed, ..
            } => {
                assert_eq!(rounds_completed, 3);
            }
            other => panic!("expected NoConsensus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn concurrent_finish_signal_aborts_remaining() {
        let (cfg, bus) = mk_config(DiscussionMode::Concurrent, 5, 1);
        // 第一个 agent (a) 调 finish_discussion
        {
            let b = bus.clone();
            b.route(DiscussionMessage {
                id: Default::default(),
                discussion_id: b.discussion_id(),
                from: AgentId("a".into()),
                kind: MessageKind::Finish,
                content: "done".into(),
                recipients: vec![],
                round: 0,
                token_usage: Default::default(),
            })
            .await
            .unwrap();
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_concurrent(
                |_a, _r, _s| async { Ok(()) },
                noop_on_event as fn(OrchestratorEvent),
            )
            .await
            .unwrap();
        match r {
            DiscussionResult::Finished { by, .. } => assert_eq!(by, AgentId("a".into())),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_snapshot_only_includes_relevant_messages() {
        // 顺序模式 prompt_for 收到 snapshot;b 应该看到 a 给 b 的单播 + 所有广播
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 1, 1);
        {
            let b = bus.clone();
            // a → b 单播
            b.route(DiscussionMessage {
                id: Default::default(),
                discussion_id: b.discussion_id(),
                from: AgentId("a".into()),
                kind: MessageKind::Utterance,
                content: "private to b".into(),
                recipients: vec![AgentId("b".into())],
                round: 0,
                token_usage: Default::default(),
            })
            .await
            .unwrap();
        }
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let snapshots = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<(
            AgentId,
            Vec<DiscussionMessage>,
        )>::new()));
        let snapshots_c = snapshots.clone();
        let _ = rt
            .run_sequential(
                move |agent, _round, snapshot| {
                    let snapshots = snapshots_c.clone();
                    let agent_c = agent.clone();
                    async move {
                        snapshots.lock().push((agent_c, snapshot));
                        Ok(())
                    }
                },
                noop_on_event,
            )
            .await;
        // 拿 a/b/c 的 snapshot 断言
        let snap = snapshots.lock().clone();
        let b_snap = snap
            .iter()
            .find(|(a, _)| a == &AgentId("b".into()))
            .unwrap();
        // b 的 snapshot 应该看到 a → b 的单播
        assert!(
            b_snap.1.iter().any(|m| m.content == "private to b"),
            "b should see the unicast from a, got: {:?}",
            b_snap.1
        );
        // c 的 snapshot 不应该看到 a → b 的单播
        let c_snap = snap
            .iter()
            .find(|(a, _)| a == &AgentId("c".into()))
            .unwrap();
        assert!(
            c_snap.1.iter().all(|m| m.content != "private to b"),
            "c should not see the unicast from a → b, got: {:?}",
            c_snap.1
        );
    }

    #[tokio::test]
    async fn prompt_builder_error_propagates() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 2, 1);
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let r = rt
            .run_sequential(
                |_a, _r, _s| async { Err(RuntimeError::PromptBuilder("boom".into())) },
                noop_on_event,
            )
            .await;
        // run_sequential 直接 ? 透传 RuntimeError,不二次包装。
        match r {
            Err(RuntimeError::PromptBuilder(s)) => assert_eq!(s, "boom"),
            other => panic!("expected PromptBuilder error, got {other:?}"),
        }
    }

    // ── v0.2.3 新增:round 计数器每轮 store ──────────────────────────────

    /// v0.2.3 起:run_sequential 在每轮开始前 `round_counter.store(round)`,
    /// 测试通过闭包捕获的 `Arc<AtomicU32>` 在每次 prompt_for 被调时观察。
    #[tokio::test]
    async fn sequential_round_counter_advances_per_round() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 3, 1);
        let rt = DiscussionRuntime::with_default_round(cfg.clone(), bus);
        let observed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u32>::new()));
        let observed_c = observed.clone();
        let counter = rt.round_counter.clone();
        let _ = rt
            .run_sequential(
                move |_agent, round, _snap| {
                    let observed = observed_c.clone();
                    let counter = counter.clone();
                    async move {
                        observed.lock().push(counter.load(Ordering::SeqCst));
                        assert_eq!(round, counter.load(Ordering::SeqCst));
                        Ok(())
                    }
                },
                noop_on_event,
            )
            .await;
        // 3 round × 3 agent = 9 次 prompt_for 调用;counter 在每轮开始前
        // store(round),所以前 3 次都是 0,接下来 3 次是 1,最后 3 次是 2。
        let observed = observed.lock().clone();
        assert_eq!(
            observed.len(),
            9,
            "expected 9 spawns total, got {observed:?}"
        );
        assert_eq!(&observed[0..3], &[0, 0, 0], "round 0 window");
        assert_eq!(&observed[3..6], &[1, 1, 1], "round 1 window");
        assert_eq!(&observed[6..9], &[2, 2, 2], "round 2 window");
    }

    /// v0.2.3 起:run_concurrent 同样在每轮 store;测试验证每轮开始时 counter
    /// 同步刷新到该轮 round。
    #[tokio::test]
    async fn concurrent_round_counter_advances_per_round() {
        let (cfg, bus) = mk_config(DiscussionMode::Concurrent, 2, 1);
        let rt = DiscussionRuntime::with_default_round(cfg.clone(), bus);
        let observed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u32>::new()));
        let observed_c = observed.clone();
        let counter = rt.round_counter.clone();
        let _ = rt
            .run_concurrent(
                move |_agent, round, _snap| {
                    let observed = observed_c.clone();
                    let counter = counter.clone();
                    async move {
                        observed.lock().push(counter.load(Ordering::SeqCst));
                        assert_eq!(round, counter.load(Ordering::SeqCst));
                        Ok(())
                    }
                },
                noop_on_event as fn(OrchestratorEvent),
            )
            .await;
        // 2 round × 3 agent = 6 次,前 3 次 round=0,后 3 次 round=1。
        let observed = observed.lock().clone();
        assert_eq!(observed.len(), 6, "expected 6 spawns, got {observed:?}");
        assert!(observed[0..3].iter().all(|r| *r == 0));
        assert!(observed[3..6].iter().all(|r| *r == 1));
    }

    // ── v0.2.3 新增:OrchestratorEvent::AgentTurn emit ──────────────────

    /// v0.2.3 起:run_sequential 在每次 prompt_for 返回 Ok 后 emit
    /// `OrchestratorEvent::AgentTurn { agent, round }`;测试收集事件验证
    /// 3 agent × 2 round = 6 次 emit,顺序与 prompt_for 调用顺序一致。
    #[tokio::test]
    async fn sequential_emits_agent_turn_after_each_spawn() {
        let (cfg, bus) = mk_config(DiscussionMode::Sequential, 2, 1);
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let events: std::sync::Arc<parking_lot::Mutex<Vec<OrchestratorEvent>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events_c = events.clone();
        let r = rt
            .run_sequential(
                |_a, _r, _s| async { Ok(()) },
                move |ev| {
                    events_c.lock().push(ev);
                },
            )
            .await
            .unwrap();
        let events = events.lock().clone();
        // 顺序模式:每轮 3 agent × 2 round = 6 次 AgentTurn(无 Finish 因为
        // 无人调 finish_discussion,orchestrator 不在 runtime 上下文 emit
        // Finished —— Finished 由 DiscussionOrchestrator::run 在外层 emit)。
        let agent_turns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, OrchestratorEvent::AgentTurn { .. }))
            .collect();
        assert_eq!(
            agent_turns.len(),
            6,
            "expected 6 AgentTurn events (3 agents × 2 rounds), got {events:?}"
        );
        // 顺序应该是 a→b→c→a→b→c(round 0 + round 1)
        let sequence: Vec<(String, u32)> = agent_turns
            .iter()
            .map(|e| match e {
                OrchestratorEvent::AgentTurn { agent, round } => (agent.0.clone(), *round),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            sequence,
            vec![
                ("a".to_string(), 0),
                ("b".to_string(), 0),
                ("c".to_string(), 0),
                ("a".to_string(), 1),
                ("b".to_string(), 1),
                ("c".to_string(), 1),
            ],
        );
        // DiscussionResult 由 orchestrator 收尾, runtime 这里返回即可
        assert!(matches!(
            r,
            DiscussionResult::NoConsensus {
                rounds_completed: 2,
                ..
            }
        ));
    }

    /// v0.2.3 起:run_concurrent 同样在每次 spawn+drain 后 emit AgentTurn;
    /// 顺序不固定(由 JoinSet 完成顺序决定),但总次数 = 3 agent × 2 round = 6。
    #[tokio::test]
    async fn concurrent_emits_agent_turn_after_each_spawn() {
        let (cfg, bus) = mk_config(DiscussionMode::Concurrent, 2, 1);
        let rt = DiscussionRuntime::with_default_round(cfg, bus);
        let events: std::sync::Arc<parking_lot::Mutex<Vec<OrchestratorEvent>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events_c = events.clone();
        let r = rt
            .run_concurrent(
                |_a, _r, _s| async { Ok(()) },
                move |ev| {
                    events_c.lock().push(ev);
                },
            )
            .await
            .unwrap();
        let events = events.lock().clone();
        let agent_turns: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, OrchestratorEvent::AgentTurn { .. }))
            .collect();
        assert_eq!(
            agent_turns.len(),
            6,
            "expected 6 AgentTurn events (3 agents × 2 rounds), got {events:?}"
        );
        // 每轮恰好 3 个 AgentTurn,round ∈ {0, 1}
        let round_counts: std::collections::HashMap<u32, usize> =
            agent_turns.iter().fold(Default::default(), |mut m, e| {
                if let OrchestratorEvent::AgentTurn { round, .. } = e {
                    *m.entry(*round).or_default() += 1;
                }
                m
            });
        assert_eq!(round_counts.get(&0).copied(), Some(3));
        assert_eq!(round_counts.get(&1).copied(), Some(3));
        // 每个 agent 在每轮都恰好 emit 1 次
        let agent_counts: std::collections::HashMap<String, usize> =
            agent_turns.iter().fold(Default::default(), |mut m, e| {
                if let OrchestratorEvent::AgentTurn { agent, .. } = e {
                    *m.entry(agent.0.clone()).or_default() += 1;
                }
                m
            });
        for a in ["a", "b", "c"] {
            assert_eq!(agent_counts.get(a).copied(), Some(2), "agent {a} count");
        }
        assert!(matches!(
            r,
            DiscussionResult::NoConsensus {
                rounds_completed: 2,
                ..
            }
        ));
    }
}
