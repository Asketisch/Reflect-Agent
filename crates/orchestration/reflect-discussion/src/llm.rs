//! `llm` — 把 `SubAgentFactory` 接入 [`DiscussionOrchestrator::run`] 的
//! `prompt_for` 闭包(v0.2.x 实战 LLM 集成层)。
//!
//! ## 设计要点
//!
//! - **职责单一**:`DiscussionOrchestrator` 保持对 LLM 无感知;本模块提供
//!   一个 boxed 闭包,满足 `Send + 'static + Clone` bound,内部执行
//!   `factory.spawn → SpawnedChild::collect_result → Ok(())`。
//! - **复用既有信号**:LLM 通过 `SendMessageTool` / `FinishDiscussionTool`
//!   工具调用表达意图(消息路由发生在 spawned agent 内部的工具调用阶段,
//!   先于 terminal event)。`collect_result` 拿到的最终 assistant 文本**丢弃**
//!   —— 这样不引入额外的 transcript-routing 双写,共识 / 主动结束检测
//!   全部沿用既有逻辑。
//! - **快照渲染**:把 `Vec<DiscussionMessage>` 渲染成明文 prompt 喂给
//!   `factory.spawn(spec, vec![], user_prompt)`。等 v0.3.x 接通
//!   `SubAgentFactory::spawn` 的 `parent_tail: Vec<ChatMessage>` 后,可改
//!   为结构化消息列表。
//!
//! ## 并发深度(历史限制已解除)
//!
//! `SubAgentFactory` 的 `MAX_DEPTH`(= 16)是**并发 in-flight 上限**:
//! spawn 时 `fetch_add`,`SpawnedChild` Drop / collect 完成时释放,
//! 嵌套 spawn 共享同一计数器。对 discussion 的实际影响:
//!
//! - [`DiscussionMode::Sequential`]:每轮只有 1 个 spawn,任意 round 数 OK。
//! - [`DiscussionMode::Concurrent`]:每轮 N 个 participant 并发, participant
//!   数(含嵌套)≤ 16 即可;超出返回 `MaxDepthExceeded`。
//!
//! (v1.5 注:本注释曾长期描述「MAX_DEPTH=3 且不在 terminal 自减」的旧行为,
//! 该行为自 in-flight 语义重构后已不存在,此处更正以免误导。)
//!
//! ## 用法
//!
//! ```ignore
//! use std::sync::Arc;
//! use reflect_discussion::llm::{build_context, prompt_for_closure};
//! use reflect_subagent::SubAgentFactory;
//!
//! let factory = Arc::new(SubAgentFactory::new(/* 构造参数 */));
//! let ctx = build_context(factory, "topic", &participants, &agent_sections, usage)?;  // 构建上下文
//! let prompt_for = prompt_for_closure(ctx, bus.clone());  // 构造 prompt 闭包
//! let result = orch.run(prompt_for, |_| {}).await?;       // 运行讨论
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use parking_lot::Mutex;
use reflect_protocol::TokenUsage;
use reflect_subagent::{DataTransferConfig, SubAgentFactory, SubAgentSpec};
use thiserror::Error;
use tracing::{debug, warn};

use crate::cli::AgentSection;
use crate::message_bus::MessageBus;
use crate::models::{AgentId, DiscussionMessage};
use crate::runtime::RuntimeError;

/// 一次讨论的 LLM wiring 上下文:spec 表 + 主题 + 共享工厂 + per-agent usage 槽。
///
/// 字段都是 `Clone` 友好的(`Arc<SubAgentFactory>` + `HashMap` + `String` +
/// `Arc<Mutex<>>`),闭包捕获时整体 clone 一次即可,满足 `Send + 'static + Clone` bound。
#[derive(Clone)]
pub struct LlmContext {
    /// 每个 participant 的 [`SubAgentSpec`](`AgentId → spec` 1:1)。
    pub specs: HashMap<AgentId, SubAgentSpec>,
    /// 讨论主题;注入每轮 prompt 头部。
    pub topic: String,
    /// 共享 [`SubAgentFactory`] —— 所有 spawned agent 共享同一个 depth
    /// 计数器,所以**整个 discussion 期间最多 3 次 spawn**(详见模块
    /// 级 rustdoc 「已知限制」段)。
    pub factory: Arc<SubAgentFactory>,
    /// v0.2.4 起: 每个 participant 对应一个 `Arc<Mutex<Option<TokenUsage>>>` 槽,
    /// 与 `DiscussionToolSet` 共享(spawning thread 在 `collect_result_with_usage`
    /// 之后写入,`SendMessageTool.execute()` 时复制到出站 `DiscussionMessage`)。
    /// 闭包通过这里的引用写回。
    pub toolset_usage: HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>>,
}

impl std::fmt::Debug for LlmContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmContext")
            .field("specs", &self.specs.keys().collect::<Vec<_>>())
            .field("topic", &self.topic)
            .field("factory", &"<SubAgentFactory>")
            .field(
                "toolset_usage",
                &self.toolset_usage.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// [`build_context`] 错误。
#[derive(Debug, Error)]
pub enum LlmError {
    /// `SubAgentSpec::validate` 失败 —— 典型如 `role` 含大写字母。
    #[error("invalid spec for role '{role}': {reason}")]
    SpecInvalid { role: String, reason: String },
    /// `DiscussionConfig.participants` 中某个 `AgentId` 在 `agents` 列表里
    /// 找不到对应 spec。
    #[error("participant '{0}' has no AgentSection in config")]
    MissingParticipant(String),
}

/// 从 TOML `AgentSection[]` + `DiscussionConfig.participants` 构建 [`LlmContext`]。
///
/// 校验:
/// - 每个 `AgentSection.role` 必须通过 [`SubAgentSpec::validate`];
/// - 每个 `participants[i]` 必须能在 `agents` 里找到对应 `AgentSection`
///   (按 `role` 匹配)。
///
/// v0.2.3 起:对每个 spec 检查 `allowed_tools` 是否包含三个 comm_tools
/// (`send_message` / `read_messages` / `finish_discussion`),缺失时
/// `tracing::warn!` 并自动 append。这样 TOML typo 不再让 agent 拿到空
/// schema 静默运行 —— 至少能跑出 transcript,日志里能看见修正轨迹。
///
/// v0.2.4 起:显式接收 `toolset_usage` 映射(per-agent usage 槽)并放进
/// `LlmContext`,让 `prompt_for_closure` 闭包在 `collect_result_with_usage`
/// 之后写回。`build_context` 不再为 slots 提供默认 —— 调用方必须提供,
/// 保证 factory + toolset + llm_context 三者对同一组 Arc clone 持有。
pub fn build_context(
    factory: Arc<SubAgentFactory>,
    topic: impl Into<String>,
    participants: &[AgentId],
    agents: &[AgentSection],
    toolset_usage: HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>>,
) -> Result<LlmContext, LlmError> {
    let mut specs: HashMap<AgentId, SubAgentSpec> = HashMap::with_capacity(agents.len());
    for a in agents {
        let spec = SubAgentSpec {
            name: a.role.clone(),
            role: a.role.clone(),
            model: None, // 沿用 factory 的 default_model
            system_prompt: a.system_prompt.clone(),
            allowed_tools: a.allowed_tools.clone(),
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        spec.validate().map_err(|reason| LlmError::SpecInvalid {
            role: a.role.clone(),
            reason,
        })?;
        specs.insert(AgentId(a.role.clone()), spec);
    }

    // 校验每个 participant 都覆盖到(防止 typo 让某个 agent 静默 fallback 到空 spec)
    for p in participants {
        if !specs.contains_key(p) {
            return Err(LlmError::MissingParticipant(p.0.clone()));
        }
    }

    // v0.2.4 起:toolset_usage 覆盖所有 participant(防止 typo 让某 agent 拿到
    // 空槽 → usage 写不进去 → 讨论里看不见 cost)。
    for p in participants {
        if !toolset_usage.contains_key(p) {
            return Err(LlmError::MissingParticipant(format!(
                "{p}: no toolset_usage slot (cli wiring bug)"
            )));
        }
    }

    // v0.2.3 起:allowed_tools 交叉校验 —— 缺失 comm_tools 时 warn 并自动补齐。
    // 不 hard error,因为允许"只想 send / 只 finish"的 agent(只用 comm_tools
    // 子集);但缺一个就 warn,避免 typo 让 agent 拿到空 schema 静默跑出
    // 无操作 Utterance。补齐策略:set-based 去重后追加 missing 名字。
    const REQUIRED_COMM_TOOLS: [&str; 3] = ["send_message", "read_messages", "finish_discussion"];
    for spec in specs.values_mut() {
        let have: HashSet<&str> = spec.allowed_tools.iter().map(String::as_str).collect();
        let missing: Vec<&str> = REQUIRED_COMM_TOOLS
            .iter()
            .copied()
            .filter(|n| !have.contains(n))
            .collect();
        if !missing.is_empty() {
            warn!(
                role = %spec.role,
                ?missing,
                "allowed_tools 缺少 comm_tools,自动补齐(可考虑显式写在 TOML 里)"
            );
            for name in missing {
                spec.allowed_tools.push(name.to_string());
            }
        }
    }

    Ok(LlmContext {
        specs,
        topic: topic.into(),
        factory,
        toolset_usage,
    })
}

/// 构造满足 `DiscussionOrchestrator::run` trait bound 的 `prompt_for` 闭包。
///
/// 返回 `impl FnMut(AgentId, u32, Vec<DiscussionMessage>) -> BoxFuture<'static,
/// Result<(), RuntimeError>> + Send + Clone + 'static`(concrete opaque 类型,
/// 每个调用点编译器生成独立 closure type);`Send + 'static + Clone` 由
/// 捕获的 `Arc<PromptForInner>`(`Arc`-backed)满足 — 闭包捕获 `Arc`,而
/// `Arc<T>: Clone` 自动派生,所以闭包本身也是 `Clone`(无需 `Box<dyn FnMut>`,
/// 那种写法 `Clone` 不能与 `FnMut` 共存)。
///
/// 闭包每次调用执行:
/// 1. 从 `ctx.specs` 取对应 `AgentId` 的 [`SubAgentSpec`](缺失则 `PromptBuilder`)。
/// 2. [`render_user_prompt`] 把 topic + agent + round + snapshot 渲染成字符串。
/// 3. `factory.spawn(spec, vec![], user_prompt).await` —— `parent_tail` 故意
///    留空(factory 当前 `_ = parent_tail` 丢弃),靠 `user_prompt` 传历史。
/// 4. v0.2.4 起:`spawned.collect_result_with_usage().await` drain `TurnHandle`
///    并抓取最后一个 `TokenCount` 事件聚合的 `TokenUsage`。写回到
///    `ctx.toolset_usage[agent]` 槽;`SendMessageTool.execute()` 后续读取
///    并翻译到出站 `DiscussionMessage.token_usage`。文本丢弃。
/// 5. `Ok(())` 让 [`crate::runtime::DiscussionRuntime`] 进入下一 agent。
pub fn prompt_for_closure(
    ctx: LlmContext,
    bus: MessageBus,
) -> impl FnMut(AgentId, u32, Vec<DiscussionMessage>) -> BoxFuture<'static, Result<(), RuntimeError>>
+ Send
+ Clone
+ 'static {
    let inner = Arc::new(PromptForInner { ctx, bus });
    move |agent, round, snapshot| {
        let inner = inner.clone();
        async move {
            let spec = inner.ctx.specs.get(&agent).cloned().ok_or_else(|| {
                RuntimeError::PromptBuilder(format!("no spec for participant '{}'", agent.0))
            })?;
            let user_prompt = render_user_prompt(&inner.ctx.topic, &agent, round, &snapshot);
            let spawned = inner
                .ctx
                .factory
                .spawn(spec, vec![], user_prompt)
                .await
                .map_err(|e| RuntimeError::PromptBuilder(format!("spawn failed: {e}")))?;
            // v0.2.4 起:抓取 token usage 写回 toolset_usage 槽,让下一条
            // SendMessageTool.execute() 把它复制到出站 DiscussionMessage。
            let result = spawned
                .collect_result_with_usage()
                .await
                .map_err(|e| RuntimeError::PromptBuilder(format!("collect_result failed: {e}")))?;
            if let (Some(slot), Some(usage)) = (
                inner.ctx.toolset_usage.get(&agent),
                result.token_usage.clone(),
            ) {
                *slot.lock() = Some(usage);
            }
            debug!(
                participant = %agent.0,
                round,
                elapsed_ms = result.elapsed_ms,
                "subagent turn completed"
            );
            // 文本丢弃:LLM 已通过 send_message / finish_discussion 工具把意图写入 bus
            // (comm_tools 持有另一份 bus clone);此处只需让 runtime 推进。
            let _ = inner.bus; // 保留 inner.bus 引用,防止 Rust 优化掉对 inner 的捕获
            let _ = result.text;
            Ok(())
        }
        .boxed()
    }
}

/// `prompt_for_closure` 闭包捕获的共享状态 —— `Arc<PromptForInner>` 保证
/// 每次闭包调用拿到独立 owned `inner`,不需要 `Mutex`(并发模式每个 task 各
/// clone 一次)。
struct PromptForInner {
    ctx: LlmContext,
    /// `_bus` 当前不直接使用 —— 它通过类型持有让闭包与 bus 实例绑定;真正的
    /// transcript 写入走 spawned agent 内部的 `SendMessageTool` /
    /// `FinishDiscussionTool`(它们持有 bus 的另一份 clone)。
    bus: MessageBus,
}

/// 把 `Vec<DiscussionMessage>` 渲染成 LLM 友好的明文 prompt。
///
/// 格式(模板字符串为运行时产品文案,保持英文):
/// ```text
/// Topic: <topic>                        # 讨论主题
/// You are: <agent>                     # 当前 participant 的角色
/// Round: <round>                       # 当前是第几轮
///
/// Visible transcript so far:
/// [r<round>/<kind>] <from>: <content>  # 每条历史消息(可重复多行)
/// [r<round>/<kind>] <from>: <content>
/// ...(省略其余消息行)
///
/// Use send_message to broadcast your reply.
/// Use read_messages to drain your inbox.
/// Use finish_discussion when consensus is reached.
/// ```
///
/// snapshot 为空时省略 history 段(避免空行噪音)。
fn render_user_prompt(
    topic: &str,
    agent: &AgentId,
    round: u32,
    snapshot: &[DiscussionMessage],
) -> String {
    let history: String = if snapshot.is_empty() {
        String::from("(no prior messages)")
    } else {
        snapshot
            .iter()
            .map(|m| format!("[r{}/{:?}] {}: {}", m.round, m.kind, m.from.0, m.content))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Topic: {topic}\nYou are: {agent}\nRound: {round}\n\n\
         Visible transcript so far:\n{history}\n\n\
         Use send_message to broadcast your reply.\n\
         Use read_messages to drain your inbox.\n\
         Use finish_discussion when consensus is reached.",
    )
}

// ── v1.4 C3:LLM 裁判 ───────────────────────────────────────────

/// 裁判子代理的 system prompt —— 强调证据导向 + 严格 JSON 输出。
const JUDGE_SYSTEM_PROMPT: &str = "\
You are a strict discussion judge. You read the full transcript of a \
multi-agent discussion and decide whether the participants have reached a \
genuine consensus.\n\n\
RULES:\n\
1. Consensus requires substantive agreement on the core question — mere \
politeness, repeated self-declarations without engagement, or residual \
disagreement on key points means NOT agreed.\n\
2. If agreed, `summary` must faithfully state what was agreed upon.\n\
3. If not agreed, `summary` states the current focus of disagreement and \
`blockers` lists the concrete unresolved points (each a short sentence).\n\
4. Respond with STRICT JSON only, no prose outside the object.";

/// 构造 LLM 裁判闭包:每轮 spawn 一个独立 judge 子代理(禁用全部工具,
/// `max_turns = 1`),让它通读全量 transcript 后输出结构化 JSON 裁决,
/// 解析为 [`JudgeVerdict`]。裁判沿用工厂的默认模型路由;spawn / 解析
/// 失败都以 Err(String) 返回(调用方 runtime 回退自报共识路径)。
pub fn make_judge_closure(factory: Arc<SubAgentFactory>) -> Arc<crate::models::JudgeCallback> {
    use crate::models::{JudgeVerdict, MessageKind as MK};
    Arc::new(move |round, transcript| {
        let factory = factory.clone();
        Box::pin(async move {
            // 渲染 transcript(全部消息,含轮次与发送者)。
            let mut body = String::new();
            for m in &transcript {
                let kind = match m.kind {
                    MK::Utterance => "utterance",
                    MK::Consensus => "self-reported-consensus",
                    MK::Finish => "finish",
                };
                body.push_str(&format!(
                    "[round {}] {} ({}): {}\n",
                    m.round, m.from.0, kind, m.content
                ));
            }
            let prompt = format!(
                "<transcript round={round}>\n{body}</transcript>\n\n\
                 Has the discussion reached a genuine consensus? Respond as a \
                 single JSON object: {{\"agreed\": bool, \"summary\": \"...\", \
                 \"blockers\": [\"...\"]}}. JSON only."
            );
            let spec = SubAgentSpec {
                name: "Judge".into(),
                role: "judge".into(),
                model: None, // 继承工厂默认模型(通常配为便宜模型)
                system_prompt: JUDGE_SYSTEM_PROMPT.into(),
                allowed_tools: vec![], // 裁判不需要工具,纯文本裁决
                data_transfer: DataTransferConfig::default(),
                max_turns: Some(1),
                allowed_skills: vec![],
            };
            let spawned = factory
                .spawn(spec, vec![], prompt)
                .await
                .map_err(|e| format!("judge spawn failed: {e}"))?;
            let text = spawned
                .collect_result()
                .await
                .map_err(|e| format!("judge collect failed: {e}"))?;
            JudgeVerdict::parse(&text)
                .inspect_err(|_| warn!(round, raw = %text, "judge verdict parse failed"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageKind;
    use parking_lot::Mutex;
    use reflect_llm::{
        ChatEvent, ChatRequest, CredentialPool, LlmError as ProviderLlmError, ModelClient,
        ModelRegistry, PoolEntry,
    };
    use reflect_protocol::ThreadId;
    use reflect_tools::ToolRegistry;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio_util::sync::CancellationToken;
    use {async_trait::async_trait, futures::stream};

    /// 计数器 stub:每次 stream 触发 fetch_add 一次,顺便产生一个文本 token 让
    /// StateGraph 走完 model_call → check_stop 路径,产出 `TurnComplete` 事件,
    /// 否则 `collect_result` 会卡到 channel 关闭仍拿不到 terminal event(测试
    /// `prompt_for_closure_invokes_factory_and_advances_depth` 验证 wiring
    /// 完整跑通)。
    struct CountingClient {
        spawns: Arc<AtomicU32>,
    }
    #[async_trait]
    impl ModelClient for CountingClient {
        fn name(&self) -> &str {
            "counting"
        }
        async fn stream(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<
            Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, ProviderLlmError>> + Send>>,
            ProviderLlmError,
        > {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter(vec![
                Ok(ChatEvent::MessageStart {
                    id: "m".into(),
                    model: "counting-1".into(),
                }),
                // 一个 token 让 StateGraph 把 latest_content 填上非空 → check_stop
                // 自然返回 Stop → submission_loop 推送 TurnComplete → collect_result
                // 能正常终止。
                Ok(ChatEvent::ContentDelta("ok".into())),
                Ok(ChatEvent::MessageStop),
            ])))
        }
    }

    fn mk_factory(spawns: Arc<AtomicU32>) -> Arc<SubAgentFactory> {
        let registry = Arc::new(ModelRegistry::new());
        registry.register_pool(
            "counting",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: Arc::new(CountingClient { spawns }),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        let cancel = CancellationToken::new();
        let tools = Arc::new(ToolRegistry::default());
        Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "counting/counting-1",
            registry,
            None, // child_registry: 回退父级 registry
            tools,
            cancel,
            None,
        ))
    }

    fn mk_section(role: &str) -> AgentSection {
        AgentSection {
            role: role.into(),
            system_prompt: format!("you are {role}"),
            allowed_tools: vec![],
        }
    }

    // ── build_context(构建上下文)───────────────────────────────────────

    /// 构造一个空 toolset_usage map,每个 participant 一个空槽。
    fn mk_usage(participants: &[AgentId]) -> HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> {
        participants
            .iter()
            .map(|p| (p.clone(), Arc::new(Mutex::new(None))))
            .collect()
    }

    #[test]
    fn build_context_rejects_unknown_participant() {
        let factory = mk_factory(Arc::new(AtomicU32::new(0)));
        let participants = vec![AgentId("advocate".into()), AgentId("ghost".into())];
        let agents = vec![mk_section("advocate")];
        let usage = mk_usage(&participants);
        let err = build_context(factory, "t", &participants, &agents, usage).unwrap_err();
        match err {
            LlmError::MissingParticipant(p) => assert_eq!(p, "ghost"),
            other => panic!("expected MissingParticipant, got {other:?}"),
        }
    }

    #[test]
    fn build_context_rejects_invalid_role() {
        let factory = mk_factory(Arc::new(AtomicU32::new(0)));
        let participants = vec![AgentId("BadCase".into())];
        let agents = vec![mk_section("BadCase")]; // 大写字母违反 validate
        let usage = mk_usage(&participants);
        let err = build_context(factory, "t", &participants, &agents, usage).unwrap_err();
        match err {
            LlmError::SpecInvalid { role, .. } => assert_eq!(role, "BadCase"),
            other => panic!("expected SpecInvalid, got {other:?}"),
        }
    }

    #[test]
    fn build_context_accepts_well_formed_sections() {
        let factory = mk_factory(Arc::new(AtomicU32::new(0)));
        let participants = vec![AgentId("a".into()), AgentId("b".into())];
        let agents = vec![mk_section("a"), mk_section("b")];
        let usage = mk_usage(&participants);
        let ctx = build_context(factory, "topic", &participants, &agents, usage).unwrap();
        assert_eq!(ctx.specs.len(), 2);
        assert_eq!(ctx.specs[&AgentId("a".into())].system_prompt, "you are a");
        assert_eq!(ctx.topic, "topic");
    }

    /// v0.2.4:toolset_usage 缺一个 participant 时 `MissingParticipant` 报错,
    /// 防止 typo 让 usage 写不进去 → 讨论里看不见 cost。
    #[test]
    fn build_context_rejects_missing_usage_slot() {
        let factory = mk_factory(Arc::new(AtomicU32::new(0)));
        let participants = vec![AgentId("a".into()), AgentId("b".into())];
        let agents = vec![mk_section("a"), mk_section("b")];
        // 只给 "a" 一个槽,"b" 漏
        let usage: HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> =
            [(AgentId("a".into()), Arc::new(Mutex::new(None)))]
                .into_iter()
                .collect();
        let err = build_context(factory, "t", &participants, &agents, usage).unwrap_err();
        match err {
            LlmError::MissingParticipant(p) => {
                assert!(p.contains("b"), "missing slot for 'b', got: {p}")
            }
            other => panic!("expected MissingParticipant, got {other:?}"),
        }
    }

    /// v0.2.3 起:`allowed_tools` 缺 `send_message` / `read_messages` /
    /// `finish_discussion` 时,build_context 记录 `tracing::warn!` 并 append
    /// 缺失名字,避免 agent 拿到空 schema 静默跑出无操作 Utterance。
    #[test]
    fn build_context_auto_fills_missing_comm_tools() {
        let factory = mk_factory(Arc::new(AtomicU32::new(0)));
        // agent "a" 只声明了 send_message,缺另外两个
        let mut section = mk_section("a");
        section.allowed_tools = vec!["send_message".into()];
        let participants = vec![AgentId("a".into())];
        let usage = mk_usage(&participants);
        let ctx =
            build_context(factory.clone(), "topic", &participants, &[section], usage).unwrap();
        let spec = &ctx.specs[&AgentId("a".into())];
        assert!(
            spec.allowed_tools.contains(&"send_message".to_string()),
            "send_message was already there"
        );
        assert!(
            spec.allowed_tools.contains(&"read_messages".to_string()),
            "read_messages auto-filled"
        );
        assert!(
            spec.allowed_tools
                .contains(&"finish_discussion".to_string()),
            "finish_discussion auto-filled"
        );
        // 验证非 comm_tools 的 allowed_tools 不被误删
        let mut section_b = mk_section("b");
        section_b.allowed_tools = vec!["some_other_tool".into(), "send_message".into()];
        let participants_b = vec![AgentId("b".into())];
        let usage_b = mk_usage(&participants_b);
        let ctx_b =
            build_context(factory, "topic", &participants_b, &[section_b], usage_b).unwrap();
        let spec_b = &ctx_b.specs[&AgentId("b".into())];
        assert!(
            spec_b
                .allowed_tools
                .contains(&"some_other_tool".to_string())
        );
        assert_eq!(spec_b.allowed_tools.len(), 4, "1 + 3 comm_tools = 4");
    }

    // ── render_user_prompt(渲染用户 prompt)──────────────────────────────

    #[test]
    fn render_user_prompt_includes_topic_role_round_history() {
        let snapshot = vec![
            DiscussionMessage {
                id: Default::default(),
                discussion_id: Default::default(),
                from: AgentId("advocate".into()),
                kind: MessageKind::Utterance,
                content: "async is better".into(),
                recipients: vec![],
                round: 0,
                token_usage: Default::default(),
            },
            DiscussionMessage {
                id: Default::default(),
                discussion_id: Default::default(),
                from: AgentId("skeptic".into()),
                kind: MessageKind::Utterance,
                content: "no, sync is simpler".into(),
                recipients: vec![],
                round: 0,
                token_usage: Default::default(),
            },
        ];
        let rendered =
            render_user_prompt("async vs sync", &AgentId("moderator".into()), 1, &snapshot);
        assert!(
            rendered.contains("Topic: async vs sync"),
            "missing topic: {rendered}"
        );
        assert!(
            rendered.contains("You are: moderator"),
            "missing agent: {rendered}"
        );
        assert!(rendered.contains("Round: 1"), "missing round: {rendered}");
        assert!(
            rendered.contains("advocate: async is better"),
            "missing message 1: {rendered}"
        );
        assert!(
            rendered.contains("skeptic: no, sync is simpler"),
            "missing message 2: {rendered}"
        );
        assert!(
            rendered.contains("send_message"),
            "missing tool hint: {rendered}"
        );
        assert!(
            rendered.contains("finish_discussion"),
            "missing finish hint: {rendered}"
        );
    }

    #[test]
    fn render_user_prompt_handles_empty_snapshot() {
        let rendered = render_user_prompt("t", &AgentId("a".into()), 0, &[]);
        assert!(rendered.contains("(no prior messages)"));
    }

    // ── prompt_for_closure 集成(wiring smoke) ────────────────────────────

    #[tokio::test]
    async fn prompt_for_closure_invokes_factory_and_advances_depth() {
        // 验证 wiring 真触发 spawn:每次闭包调用 → factory.depth() += 1
        let spawns = Arc::new(AtomicU32::new(0));
        let factory = mk_factory(spawns.clone());
        let participants = vec![
            AgentId("a".into()),
            AgentId("b".into()),
            AgentId("c".into()),
        ];
        let agents: Vec<AgentSection> = participants.iter().map(|p| mk_section(&p.0)).collect();
        let usage = mk_usage(&participants);
        let ctx = build_context(factory.clone(), "topic", &participants, &agents, usage).unwrap();

        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_c = captured.clone();
        let bus = MessageBus::new(Default::default(), participants.clone(), 4);

        let mut prompt_for = prompt_for_closure(ctx, bus);
        for agent in &participants {
            let snap: Vec<DiscussionMessage> = vec![];
            prompt_for(agent.clone(), 0, snap).await.unwrap();
            captured_c.lock().push(agent.0.clone());
        }

        // v0.x:in-flight 语义下,每次 `prompt_for` 调用内 spawn 后立即
        // collect_result,槽位同步释放。3 次连续调用终态 in-flight = 0
        // (而非旧语义下累积到 3)。
        assert_eq!(
            factory.depth(),
            0,
            "3 spawns + 3 collect_results → in-flight 归 0"
        );
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            3,
            "ModelClient invoked 3 times"
        );
        assert_eq!(captured.lock().len(), 3, "closure invoked 3 times");
    }

    /// 用 MessageKind 字段证明 DiscussionMessage 的 Debug 渲染会拼成 `[r0/Utterance] from: ...`。
    #[test]
    fn render_user_prompt_uses_debug_format_for_kind() {
        let snapshot = vec![DiscussionMessage {
            id: Default::default(),
            discussion_id: Default::default(),
            from: AgentId("a".into()),
            kind: MessageKind::Consensus,
            content: "agree".into(),
            recipients: vec![],
            round: 0,
            token_usage: Default::default(),
        }];
        let rendered = render_user_prompt("t", &AgentId("a".into()), 0, &snapshot);
        assert!(
            rendered.contains("[r0/Consensus]"),
            "Debug 渲染应为 `Consensus`(大写首字母),实际: {rendered}"
        );
    }
}

// (上面 import 块里没用到的 `Future` 已移除——`async move {}` 自动推导 impl Future。)
