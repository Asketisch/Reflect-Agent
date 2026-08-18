# `reflect-discussion`

> M9 (2026-06-22) 上线,v0.2.3 收尾实战 LLM wiring(round 标记 + AgentTurn emit + lib re-export + docs 去 hedging)。Reflect 多 Agent 讨论子系统。

## 职责

把多 Agent 讨论状态机封装成一个 `DiscussionOrchestrator`:给定 `DiscussionConfig`(`mode` / `participants` / `topic` / `consensus_window` / `max_rounds`)+ 共享 `MessageBus`,跑完整个讨论,emit `OrchestratorEvent` 观察点,返回 `DiscussionResult { Consensus | NoConsensus | Finished }`。

v0.2.3 起:每轮每 agent 通过 `prompt_for_closure` 调真实 LLM(经 `SubAgentFactory::spawn` + drain `TurnHandle`);`run_noop` 保留供测试 / 离线降级。

## Quickstart(v0.2.3 LLM 路径)

```rust
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use parking_lot::Mutex;
use reflect::{DiscussionConfig, DiscussionId, DiscussionResult, SubAgentFactory, SubAgentSpec, DataTransferConfig};
use reflect_discussion::llm::{build_context, prompt_for_closure};
use reflect_discussion::message_bus::MessageBus;
use reflect_discussion::models::{AgentId, DiscussionMode};
use reflect_discussion::tool::DiscussionToolSet;
use reflect_discussion::{AgentSection, DiscussionOrchestrator, OrchestratorEvent};
use reflect_llm::{ModelRegistry, SharedModelRegistry};
use reflect_protocol::ThreadId;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

# async fn run() {
let config = DiscussionConfig {
    mode: DiscussionMode::Sequential,
    participants: vec![AgentId("advocate".into()), AgentId("skeptic".into())],
    topic: "Async vs sync?".into(),
    consensus_window: 1,
    max_rounds: 5,
    mailbox_capacity: 32,
};
let bus = MessageBus::new(DiscussionId::new(), config.participants.clone(), 32);

// 构造 LLM registry + parent tool registry + SubAgentFactory
let registry: SharedModelRegistry = Arc::new(ModelRegistry::new());
// registry.register("openai", Arc::new(OpenAIClient::new(...)));  // or anthropic
let finished = Arc::new(Mutex::new(false));
// v0.2.3 起:共享 round_counter 给每个 DiscussionToolSet + orchestrator,
// runtime 在每轮 store(round),comm_tools execute 时 load 写入 DiscussionMessage.round。
let round_counter = Arc::new(AtomicU32::new(0));
let parent_tools = Arc::new(ToolRegistry::default());
for p in &config.participants {
    let set = DiscussionToolSet::new(p.clone(), bus.clone(), finished.clone(), round_counter.clone());
    set.verify()?;
    for name in set.tool_names() {
        if let Some(t) = set.registry.get(&name) { parent_tools.register(t); }
    }
}
let factory = Arc::new(SubAgentFactory::new(
    ThreadId::new(), "openai/gpt-4o", registry, parent_tools,
    CancellationToken::new(), None));

// 构造 LlmContext + prompt_for_closure
let agents: Vec<AgentSection> = config.participants.iter().map(|p| AgentSection {
    role: p.0.clone(),
    system_prompt: format!("You are {p} of the discussion."),
    allowed_tools: vec!["send_message".into(), "read_messages".into(), "finish_discussion".into()],
}).collect();
let ctx = build_context(factory, config.topic.clone(), &config.participants, &agents)?;

// 跑(传入同一个 round_counter)
let orch = DiscussionOrchestrator::with_round_counter(
    config, bus.clone(), Some(factory), CancellationToken::new(), None, round_counter)?;
let result: DiscussionResult = orch.run(
    prompt_for_closure(ctx, bus),
    |event| { let _ = event; },
).await?;
# Ok::<(), Box<dyn std::error::Error>>(())
# }
```

CLI 等价(`try_build_llm_orchestrator` 自动接管 wiring):

```bash
reflect discussion run -c crates/reflect-discussion/examples/discussion.toml
```

CLI 检测 provider(`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `~/.reflect/config.toml`)可用时跑真 LLM,不可用时降级 `run_noop`(offline / CI 友好)。

## 模块图

```
models        — 核心数据类型(DiscussionId / MessageId / AgentId / DiscussionMessage / DiscussionConfig / DiscussionResult)
  ↓
message_bus   — 进程内 mpsc 路由 + transcript 持久化(MessageBus / AgentMailbox / BusError)
  ↓
comm_tools    — LLM 可见的 3 个通信工具(SendMessageTool / ReadMessagesTool / FinishDiscussionTool)
  ↓
tool          — DiscussionToolSet::new 把 3 个工具注册到 ToolSource::Builtin
  ↓
runtime       — 顺序 / 并发调度循环(DiscussionRuntime / StepOutcome / RuntimeError)
  ↓
orchestrator  — 主状态机(DiscussionOrchestrator / OrchestratorEvent / OrchestratorError)
  ↓
llm           — 实战 LLM 集成层(LlmContext / build_context / prompt_for_closure / LlmError)
  ↓
cli           — `reflect discussion run` CLI 入口(DiscussionToml + try_build_llm_orchestrator 自动 wiring)
```

## 关键设计

- `MessageBus` 内部 `Arc<Inner>` 共享,`&MessageBus` 跨 `await` 借用是 `Send`(避免 `parking_lot::MutexGuard` 跨 await `!Send` 的问题)
- 顺序 vs 并发:`run_sequential` 串行 await / `run_concurrent` 用 `tokio::task::JoinSet` 并行
- 共识检测:扫 transcript,在最近 `consensus_window` 轮内所有 participant 都发过 `MessageKind::Consensus` 时整组达成共识
- 主动结束:任何 `MessageKind::Finish` 消息(由 `FinishDiscussionTool` 触发)立即退出;并发模式下 `set.abort_all()` 取消剩余 task
- **v0.2.3 LLM wiring**:`prompt_for_closure` 接 `SubAgentFactory::spawn` + `SpawnedChild::collect_result`;LLM 通过 `send_message` / `finish_discussion` 工具调用表达意图,`collect_result` 拿到的 assistant 文本丢弃(避免 transcript 双写)
- **v0.2.3 round 标记**:runtime 每轮 `store(round, SeqCst)` 到共享 `Arc<AtomicU32>`,`SendMessageTool` / `FinishDiscussionTool` 在 `execute()` 时 `load` 后写入 `DiscussionMessage.round`,保证 transcript 消息带正确轮次(跨轮共识检测 `consensus_window > 1` 依赖此)
- **v0.2.3 AgentTurn emit**:`OrchestratorEvent::AgentTurn { agent, round }` 在每次 spawn+drain 后 emit,顺序模式直接转发,并发模式经 `Arc<Mutex<Vec>>` bridge 汇总
- 协议不暴露讨论状态(`docs/protocol.md §342` 的 `EventMsg::Collab*` 12 个 v0 不实现);状态全在 crate 内消化,唯一外部可见的是 `RolloutRecord::DiscussionTranscript` 持久化

## 已知限制(留 v0.2.4 / v0.3.x / v1 解决)

- **`SubAgentFactory::MAX_DEPTH = 3`** 限制**累计 spawn 数**(非并发 in-flight):
  - `Sequential` 模式任意 round OK(depth 永远 = 1)。
  - `Concurrent` 模式 N participant → 第 1 轮 depth 直接到 N;`discussion.toml` 示例 3 agent 可用 ~1 轮。
  - v0.3.x 计划:在 `SpawnedChild::collect_result` 末尾 `fetch_sub(1)` 自减,使 depth 等价于「并发 in-flight 上限」。
- **`DiscussionMessage::token_usage` 字段当前始终为 `Default::default()`**:M9 留作 billing reservation;v0.2.4 计划在 `SpawnedChild` 新增 `collect_result_with_usage()` + `build_context` 把 token 注入 `send_message` 工具调用上下文。
- `reflect discussion ls` 当前 stub;v0.2.4 接 `reflect-rollout::list_sessions`
- TUI 不可见:协议不暴露;v0.2.4 加 `EventMsg::Collab*` 三 variant
- `RolloutRecord::DiscussionTranscript` 不写 per-agent 单独 transcript;v0.2.4 加 `Vec<ThreadId> agents` 字段

## 公开入口

`reflect` lib 顶层 re-export(v0.2.3 起扩到 12 个):
- 核心 facade (M9):`DiscussionConfig` / `DiscussionId` / `DiscussionOrchestrator` / `DiscussionResult`
- 高级 API (v0.2.3):`AgentId` / `AgentSection` / `DiscussionMode` / `MessageBus` / `MessageId` / `MessageKind` / `OrchestratorEvent` / `build_context` / `prompt_for_closure`
- 关联:`SubAgentFactory` / `SubAgentSpec` / `DataTransferConfig`(M5 re-export)

`comm_tools` / `runtime` / `tool` 留给 advanced 用户走 `reflect_discussion::*` 直接拿。
