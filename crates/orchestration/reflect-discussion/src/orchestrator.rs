//! `orchestrator` — 多 Agent 讨论编排器(M9 v0.2.0)。
//!
//! 职责:
//! 1. 持有 `DiscussionConfig` + 共享 `MessageBus` + 可选 `RolloutRecorder`
//! 2. 验证配置(非空 participants、max_rounds > 0)
//! 3. 构造 `prompt_for` 闭包(v0:noop;v0.2.x 接入 `SubAgentFactory` 后:
//!    `factory.spawn(spec, ...) → drain TurnHandle → 把 LLM 输出作为
//!    `MessageKind::Utterance` 广播)
//! 4. 跑 `DiscussionRuntime::run_sequential` / `run_concurrent`
//! 5. 收尾:emit `OrchestratorEvent::Finished` + 写 `RolloutRecord::DiscussionTranscript`
//! 6. v0.2.4 新增:向可选 `event_sink` 发出 `EventMsg::Collab*` 三类事件,
//!    让 TUI / headless JSONL 能观察讨论进度。
//!
//! v0 简化:orchestrator **不直接调 LLM**;caller 通过 `prompt_for` 闭包注入
//! step 行为。这样 unit test 不需要 wiremock / StubClient 就能跑完整状态机。
//! 接入 LLM 的标准做法由 `reflect-exec` 在 PR-C 的 `reflect discussion run`
//! CLI 里提供(`DiscussionOrchestrator` + `SubAgentFactory` + 闭包共同构造)。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use reflect_protocol::{
    CollabFinishedEvent, CollabMessageEvent, CollabStartedEvent, EventMsg, RolloutRecord,
    RolloutRecorder,
};
use reflect_subagent::SubAgentFactory;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::message_bus::MessageBus;
use crate::models::{
    AgentId, DiscussionConfig, DiscussionId, DiscussionMessage, DiscussionMode, DiscussionResult,
};
use crate::runtime::{DiscussionRuntime, RuntimeError};

/// 编排器错误。
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// [`DiscussionConfig`] 非法(空 participants / max_rounds == 0)。
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// runtime 错误(透传 [`RuntimeError`])。
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),
}

/// 把 `DiscussionResult` 序列化成 `CollabFinishedEvent.outcome` 的字符串形式。
fn result_outcome_str(r: &DiscussionResult) -> &'static str {
    match r {
        DiscussionResult::Consensus { .. } => "consensus",
        DiscussionResult::NoConsensus { .. } => "no_consensus",
        DiscussionResult::Finished { .. } => "finished",
    }
}

/// 从 `DiscussionResult` 提取实际跑的轮数。Consensus / Finished 用
/// `final_round + 1`(0-based → 1-based);NoConsensus 用 `rounds_completed`
///(已经是 0-based,但 NoConsensus 通常代表跑满 max_rounds,语义上一致)。
fn result_rounds(r: &DiscussionResult) -> u32 {
    match r {
        DiscussionResult::Consensus { final_round, .. } => final_round + 1,
        DiscussionResult::NoConsensus {
            rounds_completed, ..
        } => *rounds_completed,
        DiscussionResult::Finished { final_round, .. } => final_round + 1,
    }
}

/// 编排器对外观察点(给 CLI log / 测试断言用)。
///
/// 顺序:`Started` → (若干 `AgentTurn`)* → `Finished`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorEvent {
    /// `run()` 入口发出,带讨论 id / 参与者列表 / 模式。
    Started {
        id: DiscussionId,
        participants: Vec<AgentId>,
        mode: DiscussionMode,
    },
    /// 单 agent 单轮结束(预留;v0 不 emit,v0.2.x 由 LLM 集成层
    /// [`crate::llm::prompt_for_closure`] 在每次 spawn + drain 完成后调用
    /// `on_event(AgentTurn { agent, round })` 通知 caller)。
    AgentTurn { agent: AgentId, round: u32 },
    /// 讨论跑完,带 `DiscussionResult`。
    Finished { result: DiscussionResult },
}

/// 编排器。
#[derive(Clone)]
pub struct DiscussionOrchestrator {
    pub config: DiscussionConfig,
    /// 共享 bus(`MessageBus` 自身 `Clone`)。
    pub bus: MessageBus,
    /// 可选 subagent 工厂(v0 仅做字段存储,未在 `run` 中触发 spawn;留给
    /// v0.2.x 接入 LLM 的"实际 LLM step"用)。
    pub factory: Option<Arc<SubAgentFactory>>,
    pub cancel: CancellationToken,
    pub recorder: Option<Arc<dyn RolloutRecorder>>,
    /// 共享轮次计数器(v0.2.3+);同 [`crate::tool::DiscussionToolSet`] 的
    /// `round` 字段必须为同一个 `Arc<AtomicU32>`,runtime 每轮 `store(round)`
    /// → comm_tools `execute()` 时 `load` → 写入 `DiscussionMessage.round`。
    pub round_counter: Arc<AtomicU32>,
    /// v0.2.4 新增:可选 `EventMsg::Collab*` 输出 sink。`None` 表示不发出
    /// 协议层 Collab 事件(默认 / 单测 / `run_noop` 路径)。`Arc<dyn Fn(...)>`
    /// 而非 `FnOnce`,因为 emit 多次(Started / N × Message / Finished)。
    /// 调用方负责 `Send + Sync` 与线程安全;CLI / lib facade 把它接到一个
    /// `mpsc::Sender<Event>`,再经 OS thread + `StdoutLock` 桥到 stdout
    ///(复用 M8 `reflect-exec::spawn_reload_task` 的 pattern)。
    pub event_sink: Option<Arc<dyn Fn(EventMsg) + Send + Sync>>,
}

impl std::fmt::Debug for DiscussionOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionOrchestrator")
            .field("config", &self.config)
            .field(
                "factory",
                &self.factory.as_ref().map(|_| "<SubAgentFactory>"),
            )
            .field("cancel", &self.cancel.is_cancelled())
            .field(
                "recorder",
                &self.recorder.as_ref().map(|_| "<RolloutRecorder>"),
            )
            .field("round_counter", &self.round_counter.load(Ordering::SeqCst))
            .field(
                "event_sink",
                &self.event_sink.as_ref().map(|_| "<event_sink>"),
            )
            .finish()
    }
}

impl DiscussionOrchestrator {
    /// 构造编排器;验证 config 合法性。
    ///
    /// `round_counter` 默认新建 `Arc<AtomicU32>(0)` —— 单测 / `run_noop`
    /// 场景不需要和外部共享;实战 LLM 路径(`try_build_llm_orchestrator`)
    /// 会显式传入同一个 `Arc<AtomicU32>` 给 orchestrator + 每个
    /// `DiscussionToolSet`,让 comm_tools 能读到 runtime 写入的 round。
    pub fn new(
        config: DiscussionConfig,
        bus: MessageBus,
        factory: Option<Arc<SubAgentFactory>>,
        cancel: CancellationToken,
        recorder: Option<Arc<dyn RolloutRecorder>>,
    ) -> Result<Self, OrchestratorError> {
        Self::with_round_counter(
            config,
            bus,
            factory,
            cancel,
            recorder,
            Arc::new(AtomicU32::new(0)),
        )
    }

    /// v0.2.3 新增:显式传入共享 `Arc<AtomicU32>` 给 orchestrator + 由其
    /// 构造的 runtime。`try_build_llm_orchestrator` 用这个版本,把同一个
    /// counter 同时塞进每个 `DiscussionToolSet` 和 orchestrator。
    pub fn with_round_counter(
        config: DiscussionConfig,
        bus: MessageBus,
        factory: Option<Arc<SubAgentFactory>>,
        cancel: CancellationToken,
        recorder: Option<Arc<dyn RolloutRecorder>>,
        round_counter: Arc<AtomicU32>,
    ) -> Result<Self, OrchestratorError> {
        Self::with_event_sink(config, bus, factory, cancel, recorder, round_counter, None)
    }

    /// v0.2.4 新增:显式传入 `event_sink`。`try_build_llm_orchestrator` 在
    /// CLI / lib facade 路径下调用这个版本,把 `mpsc::Sender<Event>` clone
    /// 包成 `Arc<dyn Fn(EventMsg) + Send + Sync>` 注入。
    pub fn with_event_sink(
        config: DiscussionConfig,
        bus: MessageBus,
        factory: Option<Arc<SubAgentFactory>>,
        cancel: CancellationToken,
        recorder: Option<Arc<dyn RolloutRecorder>>,
        round_counter: Arc<AtomicU32>,
        event_sink: Option<Arc<dyn Fn(EventMsg) + Send + Sync>>,
    ) -> Result<Self, OrchestratorError> {
        if config.participants.is_empty() {
            return Err(OrchestratorError::InvalidConfig("no participants".into()));
        }
        if config.max_rounds == 0 {
            return Err(OrchestratorError::InvalidConfig(
                "max_rounds must be > 0".into(),
            ));
        }
        Ok(Self {
            config,
            bus,
            factory,
            cancel,
            recorder,
            round_counter,
            event_sink,
        })
    }

    /// 主入口:跑完整个讨论,emit OrchestratorEvent,返回 `DiscussionResult`。
    ///
    /// 闭包 `prompt_for` 决定每轮每 agent 怎么"调 LLM"(v0 实际由 caller
    /// 注入 noop 行为;v0.2.x 接入 LLM 时由 caller 把 `factory.spawn` 包
    /// 进闭包)。
    ///
    /// v0.2.4:同步 emit `EventMsg::CollabStarted` / `CollabMessage` /
    /// `CollabFinished` 三类事件到可选 `event_sink`。`CollabMessage` 在
    /// runtime 跑完之后扫描 bus.transcript() 与跑前长度差,把新增消息
    /// 一次性 emit —— 简化版,不在 prompt_for 每轮循环里 emit,避免侵入
    /// runtime 的类型签名。
    pub async fn run<P, F, PFut>(
        &self,
        prompt_for: P,
        mut on_event: F,
    ) -> Result<DiscussionResult, OrchestratorError>
    where
        P: FnMut(AgentId, u32, Vec<crate::models::DiscussionMessage>) -> PFut
            + Send
            + 'static
            + Clone,
        PFut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
        F: FnMut(OrchestratorEvent),
    {
        let id = self.bus.discussion_id();
        on_event(OrchestratorEvent::Started {
            id,
            participants: self.config.participants.clone(),
            mode: self.config.mode,
        });
        // v0.2.4: 同步 emit `CollabStarted` —— 在 `on_event(Started)` 之后,
        // runtime 跑之前;`cancel` 检查放在 emit 之后,确保 cancellation
        // 路径不污染观察者预期。
        self.emit(EventMsg::CollabStarted(CollabStartedEvent {
            id: id.to_string(),
            participants: self
                .config
                .participants
                .iter()
                .map(|a| a.0.clone())
                .collect(),
            mode: match self.config.mode {
                DiscussionMode::Sequential => "sequential".into(),
                DiscussionMode::Concurrent => "concurrent".into(),
            },
        }));
        info!(
            ?id,
            participants = self.config.participants.len(),
            "discussion started"
        );

        // 取消信号:如果 cancel 已触发,直接返 Cancelled 错误。
        if self.cancel.is_cancelled() {
            return Err(OrchestratorError::Runtime(RuntimeError::Cancelled));
        }

        // v0.2.4: 跑前 snapshot bus.transcript() 长度,跑后 emit 新增
        // 消息为 `CollabMessage`(包含 token_usage,如 LLM 路径下注入)。
        let transcript_len_before = self.bus.transcript().len();

        let mut runtime = DiscussionRuntime::new(
            self.config.clone(),
            self.bus.clone(),
            self.round_counter.clone(),
        );
        // v1.4 C3:裁判模式 —— config.judge 且工厂在位时,构造 LLM 裁判
        // 闭包(spawn 独立 judge 子代理,禁用工具,max_turns=1)挂到
        // runtime。工厂缺席时 warn 并回退自报共识。
        if self.config.judge {
            match self.factory.clone() {
                Some(factory) => {
                    runtime = runtime.with_judge(crate::llm::make_judge_closure(factory));
                }
                None => {
                    warn!(
                        "discussion judge=true but no factory wired; falling back to self-reported consensus"
                    );
                }
            }
        }
        let result = match self.config.mode {
            // 顺序模式:on_event 闭包只需 FnMut,直接 &mut 借用即可。
            DiscussionMode::Sequential => {
                runtime
                    .run_sequential(prompt_for, |ev| on_event(ev))
                    .await?
            }
            // 并发模式:on_event 被 spawn 进 tokio task,必须满足
            // Send + 'static。用 Arc<Mutex<Vec<...>>> 桥接,任务内 push,
            // 外层 join 完再 drain 给 on_event。
            DiscussionMode::Concurrent => {
                let events_slot: Arc<parking_lot::Mutex<Vec<OrchestratorEvent>>> =
                    Arc::new(parking_lot::Mutex::new(Vec::new()));
                let slot_c = events_slot.clone();
                let r = runtime
                    .run_concurrent(prompt_for, move |ev| {
                        slot_c.lock().push(ev);
                    })
                    .await?;
                for ev in events_slot.lock().drain(..) {
                    on_event(ev);
                }
                r
            }
        };

        // v0.2.4: emit 所有新增 message 为 CollabMessage。transcript
        // 顺序 = bus.route 顺序,无论 sequential 还是 concurrent 模式都保留
        // 该全局顺序(sequential 自然;concurrent 下 bus 内 mpsc channel
        // 串行化所有 route,顺序稳定)。
        let transcript = self.bus.transcript();
        for msg in &transcript[transcript_len_before..] {
            self.emit_collab_message(id, msg);
        }

        on_event(OrchestratorEvent::Finished {
            result: result.clone(),
        });
        // v0.2.4: emit CollabFinished。`outcome` 与 `rounds` 从 result
        // 提取,顺序模式下 final_round+1 = 实际跑的轮数,concurrent 同理。
        self.emit(EventMsg::CollabFinished(CollabFinishedEvent {
            id: id.to_string(),
            outcome: result_outcome_str(&result).into(),
            rounds: result_rounds(&result),
        }));

        // 收尾:写一条 DiscussionTranscript 到 recorder(如有)。
        self.write_transcript_record(id).await;

        Ok(result)
    }

    /// v0.2.4: 把单条 `DiscussionMessage` 翻译成 `CollabMessageEvent` 并 emit。
    fn emit_collab_message(&self, id: DiscussionId, msg: &DiscussionMessage) {
        self.emit(EventMsg::CollabMessage(CollabMessageEvent {
            id: id.to_string(),
            from: msg.from.0.clone(),
            kind: match msg.kind {
                crate::models::MessageKind::Utterance => "utterance",
                crate::models::MessageKind::Consensus => "consensus",
                crate::models::MessageKind::Finish => "finish",
            }
            .into(),
            content: msg.content.clone(),
            round: msg.round,
            token_usage: msg.token_usage_as_protocol(),
        }));
    }

    /// v0.2.4: 内部 emit helper —— `event_sink` 为 `None` 时静默 noop;
    /// 否则调用 sink。
    fn emit(&self, evt: EventMsg) {
        if let Some(sink) = &self.event_sink {
            sink(evt);
        }
    }

    /// 简化入口:用 noop `prompt_for` 跑(用于测试,以及 bus 状态已经被预
    /// 注入好"达成共识"或"finish"信号的场景)。
    ///
    /// 实战应该用 [`DiscussionOrchestrator::run`] 注入真实 LLM step。
    pub async fn run_noop<F>(&self, on_event: F) -> Result<DiscussionResult, OrchestratorError>
    where
        F: FnMut(OrchestratorEvent),
    {
        self.run(|_agent, _round, _snapshot| async { Ok(()) }, on_event)
            .await
    }

    /// 把 bus 当前 transcript 写到 recorder 作为一条
    /// `RolloutRecord::DiscussionTranscript`(失败仅 warn,不中断 result)。
    async fn write_transcript_record(&self, id: DiscussionId) {
        let Some(rec) = self.recorder.as_ref() else {
            return;
        };
        let transcript_json = {
            let bus = self.bus.clone();
            match serde_json::to_value(bus.transcript()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(?e, "failed to serialize discussion transcript");
                    return;
                }
            }
        };
        let record = RolloutRecord::DiscussionTranscript {
            discussion_id: Uuid::from(id),
            mode: match self.config.mode {
                DiscussionMode::Sequential => "sequential".into(),
                DiscussionMode::Concurrent => "concurrent".into(),
            },
            participants: self
                .config
                .participants
                .iter()
                .map(|a| a.0.clone())
                .collect(),
            // v0.2.4: 整场讨论的 transcript 不带 agent 切片。per-agent slices
            // 由 `prompt_for_closure` 在每次 spawn 后单独 emit(留 v0.2.5 扩展,
            // 当前实现不拆分)。
            agent_id: None,
            transcript: transcript_json,
        };
        if let Err(e) = rec.record(record).await {
            warn!(?e, "DiscussionOrchestrator: recorder write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use crate::models::{DiscussionMessage, MessageKind};
    use reflect_protocol::NullRecorder;

    fn mk_orchestrator(
        mode: DiscussionMode,
        max_rounds: u32,
        consensus_window: u32,
    ) -> (DiscussionOrchestrator, MessageBus) {
        let participants = vec![
            AgentId("a".into()),
            AgentId("b".into()),
            AgentId("c".into()),
        ];
        let config = DiscussionConfig {
            mode,
            participants: participants.clone(),
            topic: "t".into(),
            consensus_window,
            max_rounds,
            mailbox_capacity: 4,
            judge: false,
        };
        let bus = MessageBus::new(DiscussionId::new(), participants, 4);
        let mut orch =
            DiscussionOrchestrator::new(config, bus.clone(), None, CancellationToken::new(), None)
                .unwrap();
        // orch 默认是 Send+Sync 的不可变借用;mut 让 caller 可以在测试中调
        // `orch.cancel.cancel()`(mut 方法)。
        let _ = &mut orch;
        (orch, bus)
    }

    async fn inject_consensus(bus: &MessageBus) {
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

    #[test]
    fn new_rejects_empty_participants() {
        let cfg = DiscussionConfig {
            participants: vec![],
            ..DiscussionConfig::default()
        };
        let bus = MessageBus::new(DiscussionId::new(), vec![], 4);
        let err = DiscussionOrchestrator::new(cfg, bus, None, CancellationToken::new(), None)
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidConfig(_)));
    }

    #[test]
    fn new_rejects_zero_max_rounds() {
        let cfg = DiscussionConfig {
            participants: vec![AgentId("a".into())],
            max_rounds: 0,
            ..DiscussionConfig::default()
        };
        let bus = MessageBus::new(DiscussionId::new(), vec![AgentId("a".into())], 4);
        let err = DiscussionOrchestrator::new(cfg, bus, None, CancellationToken::new(), None)
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn run_emits_started_then_finished_events() {
        let (orch, _bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        let mut events = Vec::new();
        let result = orch.run_noop(|e| events.push(e)).await.unwrap();
        // v0.2.3 起:中间 emit AgentTurn(3 个 agent 各 1 次);总共 5 个事件
        // (Started + 3 × AgentTurn + Finished)。
        assert_eq!(
            events.len(),
            5,
            "expected Started + 3 AgentTurn + Finished, got {events:?}"
        );
        assert!(matches!(events[0], OrchestratorEvent::Started { .. }));
        assert!(matches!(events[1], OrchestratorEvent::AgentTurn { .. }));
        assert!(matches!(events[2], OrchestratorEvent::AgentTurn { .. }));
        assert!(matches!(events[3], OrchestratorEvent::AgentTurn { .. }));
        assert!(matches!(events[4], OrchestratorEvent::Finished { .. }));
        // NoConsensus(0 round 内无 Consensus)
        assert!(matches!(result, DiscussionResult::NoConsensus { .. }));
    }

    #[tokio::test]
    async fn run_sequential_reaches_consensus() {
        let (orch, bus) = mk_orchestrator(DiscussionMode::Sequential, 2, 1);
        inject_consensus(&bus).await;
        let result = orch.run_noop(|_| {}).await.unwrap();
        assert!(matches!(
            result,
            DiscussionResult::Consensus { final_round: 0, .. }
        ));
    }

    #[tokio::test]
    async fn run_concurrent_reaches_consensus() {
        let (orch, bus) = mk_orchestrator(DiscussionMode::Concurrent, 2, 1);
        inject_consensus(&bus).await;
        let result = orch.run_noop(|_| {}).await.unwrap();
        assert!(matches!(
            result,
            DiscussionResult::Consensus { final_round: 0, .. }
        ));
    }

    #[tokio::test]
    async fn run_records_discussion_transcript_to_recorder() {
        let (orch, bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        inject_consensus(&bus).await;
        // 用一个能记录所有 record 的 recorder
        let recorder = Arc::new(RecordingRecorder::default());
        let orch = DiscussionOrchestrator::new(
            orch.config,
            bus.clone(),
            orch.factory.clone(),
            orch.cancel.clone(),
            Some(recorder.clone()),
        )
        .unwrap();
        let _ = orch.run_noop(|_| {}).await.unwrap();
        let records = recorder.records.lock().clone();
        assert_eq!(records.len(), 1, "expected 1 transcript record");
        match &records[0] {
            RolloutRecord::DiscussionTranscript {
                mode, participants, ..
            } => {
                assert_eq!(mode, "sequential");
                assert_eq!(
                    participants,
                    &vec!["a".to_string(), "b".to_string(), "c".to_string()]
                );
            }
            other => panic!("expected DiscussionTranscript, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_no_recorder_works() {
        let (orch, _bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        // recorder: None 不影响结果
        let r = orch.run_noop(|_| {}).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn run_with_cancelled_token_returns_cancelled() {
        let (orch, _bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        // CancellationToken::cancel(&self) — 内部用 Arc<AtomicBool>,无副作用借用。
        orch.cancel.cancel();
        let r = orch.run_noop(|_| {}).await;
        match r {
            Err(OrchestratorError::Runtime(RuntimeError::Cancelled)) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    // ── Helper: record-all recorder(给单测看 DiscussionTranscript 是否被 emit)──

    #[derive(Default, Debug)]
    struct RecordingRecorder {
        records: parking_lot::Mutex<Vec<RolloutRecord>>,
    }

    #[async_trait::async_trait]
    impl RolloutRecorder for RecordingRecorder {
        async fn record(&self, r: RolloutRecord) -> anyhow::Result<()> {
            self.records.lock().push(r);
            Ok(())
        }
        async fn replay(
            &self,
            _session_id: reflect_protocol::ThreadId,
        ) -> anyhow::Result<Vec<RolloutRecord>> {
            Ok(self.records.lock().clone())
        }
        async fn list_sessions(&self) -> anyhow::Result<Vec<reflect_protocol::SessionInfo>> {
            Ok(vec![])
        }
        async fn truncate_after(
            &self,
            _to_turn_id: Option<&reflect_protocol::TurnId>,
        ) -> anyhow::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn null_recorder_works_as_noop() {
        // 冒烟测试:NullRecorder 不报错
        let r = NullRecorder;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            r.record(RolloutRecord::session_meta(
                reflect_protocol::ThreadId::new(),
                "m",
            ))
            .await
            .unwrap();
        });
    }

    // ── v0.2.4 新增:event_sink + Collab 事件 emit 测试 ─────────────────

    /// v0.2.4:`run` 在 entry emit 1 个 `CollabStarted`,结束时 emit 1 个
    /// `CollabFinished`;event_sink 为 `None` 时不 panic。
    #[tokio::test]
    async fn run_emits_collab_started_and_finished_events() {
        let (orch, _bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        let captured: Arc<parking_lot::Mutex<Vec<EventMsg>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let cap_c = captured.clone();
        let orch = DiscussionOrchestrator::with_event_sink(
            orch.config,
            orch.bus.clone(),
            orch.factory.clone(),
            orch.cancel.clone(),
            orch.recorder.clone(),
            orch.round_counter.clone(),
            Some(Arc::new(move |evt| {
                cap_c.lock().push(evt);
            })),
        )
        .unwrap();
        let _ = orch.run_noop(|_| {}).await.unwrap();
        let events = captured.lock().clone();
        // 应当有 Started(OrchestratorEvent)+ CollabStarted(EventMsg) + ...
        // + Finished(OrchestratorEvent)+ CollabFinished(EventMsg)。
        let started = events
            .iter()
            .filter(|e| matches!(e, EventMsg::CollabStarted(_)))
            .count();
        let finished = events
            .iter()
            .filter(|e| matches!(e, EventMsg::CollabFinished(_)))
            .count();
        assert_eq!(started, 1, "expected 1 CollabStarted, got {events:?}");
        assert_eq!(finished, 1, "expected 1 CollabFinished, got {events:?}");
        // CollabStarted 内容核对
        if let EventMsg::CollabStarted(s) = &events[0] {
            assert_eq!(s.mode, "sequential");
            assert_eq!(s.participants, vec!["a", "b", "c"]);
        } else {
            panic!(
                "first captured event should be CollabStarted, got {:?}",
                events[0]
            );
        }
        // CollabFinished 内容核对(noop 路径 = NoConsensus)
        let last = events.last().expect("at least one event");
        if let EventMsg::CollabFinished(f) = last {
            assert_eq!(f.outcome, "no_consensus");
            assert_eq!(
                f.rounds, 1,
                "max_rounds=1, no_consensus → rounds_completed=1"
            );
        } else {
            panic!("last captured event should be CollabFinished, got {last:?}");
        }
    }

    /// v0.2.4:emit CollabMessage 给 transit 新增的每条消息。用一个 prompt_for
    /// closure 让每个 agent 在 turn 内 route 一条 utterance,assert 收到 3 个
    /// CollabMessage(kind=utterance)。
    #[tokio::test]
    async fn run_emits_collab_message_per_routed_message() {
        let (orch, bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        let captured: Arc<parking_lot::Mutex<Vec<EventMsg>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let cap_c = captured.clone();
        let orch_bus = bus.clone();
        let orch = DiscussionOrchestrator::with_event_sink(
            orch.config,
            orch.bus.clone(),
            orch.factory.clone(),
            orch.cancel.clone(),
            orch.recorder.clone(),
            orch.round_counter.clone(),
            Some(Arc::new(move |evt| {
                cap_c.lock().push(evt);
            })),
        )
        .unwrap();
        // prompt_for 让每个 agent 在 turn 内通过 bus.route 发一条 utterance。
        // runtime 会调 (agent, round, snapshot),我们用 snapshot 长度做 msg.id。
        let _ = orch
            .run(
                move |agent, round, _snapshot| {
                    let bus = orch_bus.clone();
                    async move {
                        bus.route(DiscussionMessage {
                            id: Default::default(),
                            discussion_id: bus.discussion_id(),
                            from: agent.clone(),
                            kind: MessageKind::Utterance,
                            content: format!("hello from {}", agent.0),
                            recipients: vec![],
                            round,
                            token_usage: Default::default(),
                        })
                        .await
                        .map_err(|_| RuntimeError::Cancelled)?;
                        Ok(())
                    }
                },
                |_| {},
            )
            .await
            .unwrap();
        let events = captured.lock().clone();
        let messages: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                EventMsg::CollabMessage(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            messages.len(),
            3,
            "expected 3 CollabMessage (one per agent), got {messages:?}"
        );
        for m in &messages {
            assert_eq!(m.kind, "utterance");
        }
        // 内容核对
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"hello from a"));
        assert!(contents.contains(&"hello from b"));
        assert!(contents.contains(&"hello from c"));
    }

    /// v0.2.4:`event_sink = None` 路径不 panic,emit helper 是 noop。
    #[tokio::test]
    async fn run_does_not_panic_when_event_sink_is_none() {
        let (orch, _bus) = mk_orchestrator(DiscussionMode::Sequential, 1, 1);
        // mk_orchestrator 默认 event_sink: None
        let r = orch.run_noop(|_| {}).await;
        assert!(r.is_ok(), "event_sink=None should not panic, got {r:?}");
    }
}
