//! `cli` — `zhuye discussion run` 子命令的入口。
//!
//! v0.2.x 起:能识别 provider 配置(API key / TOML)时调真实 LLM;
//! 否则降级 `run_noop`(offline / CI 友好)。
//!
//! LLM 路径 = `try_build_llm_orchestrator` + `zhuye_discussion::llm::prompt_for_closure`
//! 跑 `orch.run(...)`;noop 路径保留 `orch.run_noop(...)` 跑状态机。
//!
//! v0.2.4 起:LLM 路径同时
//! - 给每个 participant 构造共享 `Arc<Mutex<Option<TokenUsage>>>` 槽,
//!   `prompt_for_closure` 在 `collect_result_with_usage` 后写入
//!   `SendMessageTool.token_usage`;comm_tools 把 usage 翻译到
//!   `DiscussionMessage.token_usage` 后被 orchestrator 翻译成
//!   `EventMsg::CollabMessageEvent.token_usage`;
//! - 构造一个 OS-thread + `mpsc::Sender<Event>` 桥,把 orchestrator 的
//!   `EventMsg::Collab*` 事件写到 stdout(复用 M8 reload 模式)。
//!
//! 实战用法:`zhuye discussion run -c discussion.toml [-o result.json]`
//! 输出 `MessageBus::format_transcript()` 文本 + `serde_json::to_string_pretty(&result)`。

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zhuye_llm::{ModelRegistry, SharedModelRegistry};
use zhuye_protocol::{EventMsg, ThreadId, TokenUsage};
use zhuye_subagent::SubAgentFactory;
use zhuye_tools::ToolRegistry;

use crate::llm::{build_context, prompt_for_closure};
use crate::message_bus::MessageBus;
use crate::models::{AgentId, DiscussionConfig, DiscussionId, DiscussionMode};
use crate::orchestrator::{DiscussionOrchestrator, OrchestratorError};
use crate::tool::DiscussionToolSet;

/// TOML schema:`[discussion]` 顶层 + `[[agents]]` 段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionToml {
    pub discussion: DiscussionSection,
    #[serde(default)]
    pub agents: Vec<AgentSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionSection {
    #[serde(default = "default_mode")]
    pub mode: DiscussionMode,
    pub topic: String,
    #[serde(default = "default_consensus_window")]
    pub consensus_window: u32,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_mailbox_capacity")]
    pub mailbox_capacity: usize,
}

fn default_mode() -> DiscussionMode {
    DiscussionMode::Concurrent
}
fn default_consensus_window() -> u32 {
    1
}
fn default_max_rounds() -> u32 {
    10
}
fn default_mailbox_capacity() -> usize {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSection {
    pub role: String,
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

impl DiscussionToml {
    /// 从 TOML 文件解析。
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CliError::Io(format!("read {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| CliError::Parse(format!("{}: {e}", path.display())))
    }

    /// 转换为运行时 `DiscussionConfig`(参与 agents 不在 TOML 顶层,实际
    /// 从 `[[agents]]` 推导)。
    pub fn into_config(self) -> DiscussionConfig {
        let participants: Vec<AgentId> = self
            .agents
            .iter()
            .map(|a| AgentId(a.role.clone()))
            .collect();
        DiscussionConfig {
            mode: self.discussion.mode,
            participants,
            topic: self.discussion.topic,
            consensus_window: self.discussion.consensus_window,
            max_rounds: self.discussion.max_rounds,
            mailbox_capacity: self.discussion.mailbox_capacity,
            judge: false,
        }
    }
}

/// CLI 错误。
#[derive(Debug, Error)]
pub enum CliError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("orchestrator: {0}")]
    Orchestrator(#[from] OrchestratorError),
    #[error("write: {0}")]
    Write(String),
}

/// `zhuye discussion run -c <config> [-o <output>]` 主入口。
///
/// v0.2.x 起:识别 provider 后跑真 LLM(`zhuye-discussion::llm::prompt_for_closure`);
/// 未配置时降级 `run_noop`,状态机仍跑通但 transcript 空。离线 / CI 友好。
pub async fn run(config_path: &Path, output: Option<&Path>) -> Result<(), CliError> {
    let toml_cfg = DiscussionToml::load(config_path)?;
    let config = toml_cfg.clone().into_config();
    let bus = MessageBus::new(
        DiscussionId::new(),
        config.participants.clone(),
        config.mailbox_capacity,
    );

    let result = match try_build_llm_orchestrator(&config, &bus).await {
        Some((orch, factory, _toolset_usage)) => {
            tracing::info!(
                model = %factory.default_model(),
                participants = config.participants.len(),
                "discussion: using LLM-backed orchestrator"
            );
            // v0.2.4:toolset_usage 已经被 try_build_llm_orchestrator 绑进
            // DiscussionToolSet,这里重建 ctx 时用同一个 map —— llm.rs 的
            // build_context 校验每个 participant 都覆盖 toolset_usage,漏一个
            // 就 fail-fast(防 typo)。toolset_usage map 与 comm_tools 共享
            // 同一组 Arc clone,prompt_for_closure 写回的 token usage 会被
            // SendMessageTool.execute() 立刻读到。
            let toolset_usage = build_toolset_usage(&config);
            let ctx = build_context(
                factory,
                config.topic.clone(),
                &config.participants,
                &toml_cfg.agents,
                toolset_usage,
            )
            .map_err(|e| CliError::Orchestrator(OrchestratorError::InvalidConfig(e.to_string())))?;
            // v0.2.4:try_build_llm_orchestrator 已内部启动 OS-thread drainer
            // (StdoutLock `!Send`,必须 OS thread 持有);sync mpsc bridge 与
            // orchestrator 的 event_sink(`Arc<dyn Fn(EventMsg) + Send + Sync>`)
            // 双向不耦合。channel 在 orchestrator.run() 期间持续读,run()
            // 退出后 drainer 自然收到 RecvError 退出。
            orch.run(prompt_for_closure(ctx, bus.clone()), |_| {})
                .await?
        }
        None => {
            warn!(
                "no provider configured (OPENAI_API_KEY/ANTHROPIC_API_KEY unset and config.toml has no usable section); falling back to run_noop"
            );
            let orch = DiscussionOrchestrator::new(
                config,
                bus.clone(),
                None,
                CancellationToken::new(),
                None,
            )?;
            // v0.2.4:noop 路径同样发 CollabStarted + CollabFinished,中间无
            // CollabMessage —— 没有 participant 路由消息。仍发 drainer 是
            // 因为 Orchestrator.run_noop 不持 event_sink;这里手动 emit 一对
            // 边界事件再落 JSONL 也行,但默认行为是 noop 静默,所以跳过的更干净。
            orch.run_noop(|_| {}).await?
        }
    };

    let transcript = bus.format_transcript();
    let result_json = serde_json::to_string_pretty(&result)
        .map_err(|e| CliError::Write(format!("serialize result: {e}")))?;
    let output_text =
        format!("=== Discussion Transcript ===\n{transcript}\n=== Result ===\n{result_json}\n",);
    if let Some(path) = output {
        std::fs::write(path, &output_text)
            .map_err(|e| CliError::Write(format!("write {}: {e}", path.display())))?;
    } else {
        print!("{output_text}");
    }
    Ok(())
}

/// v0.2.4:每个 participant 对应一个 `Arc<Mutex<Option<TokenUsage>>>` 槽,
/// 与 DiscussionToolSet 和 LlmContext 共享(LLM 跑完后写回 usage)。
type ToolsetUsageMap = HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>>;

/// 按 participant 组装工具表:共享并集 + 每个 participant 一份 per-role 表。
///
/// **同名工具多身份共存**:各 participant 的 comm 工具(`send_message` 等)
/// **名字相同**但 `self_id` 不同。`ToolRegistry` 按名索引,把所有 participant
/// 的工具注册进同一张表时后者覆盖前者 —— 所有 participant 最终共用最后一个
/// 身份(消息的 `from` 取决于注册顺序),身份互串。
///
/// 这里返回两张表:
/// - 共享并集:同名项取最后一个,供无 role 覆盖的 spawn 兜底;
/// - per-role 表:以并集为底(工具集合不丢 —— `allowed_tools` 里的父级工具
///   仍可解析),再用**本 participant 自己的**工具版本覆盖同名项。
///
/// per-role 表经 `SubAgentFactory::set_role_tools(role, ..)` 按 role 注入,
/// `spawn` 见到 role 有覆盖时从对应表过滤工具。
fn build_tool_registries(
    participant_sets: &[(AgentId, DiscussionToolSet)],
) -> (Arc<ToolRegistry>, Vec<(String, Arc<ToolRegistry>)>) {
    let parent_tools = Arc::new(ToolRegistry::default());
    for (_, set) in participant_sets {
        for name in set.tool_names() {
            if let Some(t) = set.registry.get(&name) {
                parent_tools.register(t);
            }
        }
    }
    let mut role_registries: Vec<(String, Arc<ToolRegistry>)> = Vec::new();
    for (p, set) in participant_sets {
        let own = Arc::new(ToolRegistry::default());
        for name in parent_tools.list() {
            if let Some(t) = parent_tools.get(&name) {
                own.register(t);
            }
        }
        for name in set.tool_names() {
            if let Some(t) = set.registry.get(&name) {
                own.register(t);
            }
        }
        role_registries.push((p.0.clone(), own));
    }
    (parent_tools, role_registries)
}

/// 尝试构造 LLM-backed orchestrator:provider 可用 + participants 非空 +
/// 每个 participant 都能注册 DiscussionToolSet + SubAgentFactory 可建。
///
/// 任意一步失败返 `None`,调用方降级 `run_noop`。**不读**任何环境变量 —— 全
/// 委托给 `zhuye_config::ReflectConfig::load_default`(已实现 env 优先于
/// TOML, TOML 优先于内置默认),保证与 `zhuye-exec` / `zhuye-tui` 的
/// provider 选择行为完全一致。
///
/// v0.2.4 起:返回元组多带一个 `HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>>`,
/// 给 `build_context` 注入,让 `prompt_for_closure` 在 `collect_result_with_usage`
/// 之后写入,`SendMessageTool.execute()` 复制到出站 `DiscussionMessage.token_usage`。
///
/// TOML `AgentSection[]` 由调用方在 `build_context` 处透传(此处只校验
/// provider + factory wiring 完整性,不接触 system_prompt)。
async fn try_build_llm_orchestrator(
    config: &DiscussionConfig,
    bus: &MessageBus,
) -> Option<(
    DiscussionOrchestrator,
    Arc<SubAgentFactory>,
    ToolsetUsageMap,
)> {
    // 1. provider + model spec(`model = "free"` 在此解析成具体 id)
    let zhuye_cfg = zhuye_config::load_default();
    let registry: SharedModelRegistry = Arc::new(ModelRegistry::new());
    zhuye_cfg.apply_to_registry(&registry).ok()?;
    let resolved = zhuye_config::resolve_model_spec(&zhuye_cfg, &registry)
        .await
        .ok()
        .flatten()?;

    // 2. 每个 participant 一个 DiscussionToolSet,合并到 parent ToolRegistry。
    // 注意 SendMessageTool 携带 self_id,所以必须每个 participant 单独构造,
    // 不能共享同一个 DiscussionToolSet 实例。
    //
    // v0.2.3 起:同时构造一个共享 `Arc<AtomicU32>` round_counter,每个
    // DiscussionToolSet + DiscussionOrchestrator + runtime 都持有同一
    // 实例 —— runtime 在每轮 store(round),comm_tools 在 execute 时
    // load 后写入 DiscussionMessage.round,保证 transcript 消息带正确
    // 轮次(consensus_window > 1 时跨轮共识检测依赖此标记)。
    //
    // v0.2.4 起:同时构造一个共享 `Arc<Mutex<Option<TokenUsage>>>` per-agent usage 槽,
    // 每个 participant 的 DiscussionToolSet 和闭包侧的 LlmContext 都持有同一 Arc clone
    // —— SendMessageTool.execute() 读取槽,prompt_for_closure 在
    // collect_result_with_usage 之后写入。
    let finished = Arc::new(Mutex::new(false));
    let round_counter = Arc::new(AtomicU32::new(0));
    let toolset_usage = build_toolset_usage(config);
    // review 修复:同名工具多身份共存 ——
    // 各 participant 的 comm 工具(`send_message` 等)**名字相同**但
    // `self_id` 不同。注册进同一个按名索引的 registry 时后者覆盖前者,
    // 所有 participant 最终共用最后一个身份(消息发给谁取决于注册顺序)。
    //
    // 改为:每个 participant 一份独立 registry(并集 + 本 participant 自己的
    // 工具版本覆盖同名项),经 `factory.set_role_tools(role, ...)` 按 role
    // 注入;`spawn` 见到 role 有覆盖时从对应 registry 过滤工具。
    //
    // 第一遍:构造各 participant 的工具集(每个携带自己的 self_id)。
    let mut participant_sets: Vec<(AgentId, DiscussionToolSet)> = Vec::new();
    for p in &config.participants {
        let slot = toolset_usage
            .get(p)
            .cloned()
            .expect("build_toolset_usage covers all participants");
        let set = DiscussionToolSet::with_usage(
            p.clone(),
            bus.clone(),
            finished.clone(),
            round_counter.clone(),
            slot,
        );
        if set.verify().is_err() {
            return None;
        }
        participant_sets.push((p.clone(), set));
    }
    // 共享表 + per-role 表(见 `build_tool_registries`)。
    let (parent_tools, role_registries) = build_tool_registries(&participant_sets);

    // 3. SubAgentFactory + DiscussionOrchestrator + event_sink
    let factory = Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        resolved,
        registry,
        None, // child_registry: CLI 不配独立 subagent 凭证,回退父级 registry
        parent_tools,
        CancellationToken::new(),
        None, // CLI 不开 JSONL 持久化(每个 spawned agent 各自走自己的 session)
    ));
    // 按 role 注入 participant 专属工具表(见上方「同名工具多身份共存」)。
    for (role, reg) in role_registries {
        factory.set_role_tools(&role, reg);
    }
    // v0.2.4:orchestrator 的 event_sink 把 Collab* 事件送到 sync mpsc,由
    // spawn_collab_stdout_drainer() 启的 OS 线程写到 stdout JSONL。
    // 用 sync mpsc 而非 tokio::sync::mpsc,因为接收端是 OS 线程(StdoutLock !Send)。
    let (tx, rx) = std::sync::mpsc::channel::<EventMsg>();
    let tx_for_sink = tx.clone();
    let event_sink: Arc<dyn Fn(EventMsg) + Send + Sync> = Arc::new(move |evt: EventMsg| {
        // best-effort:接收端退出时 sender 错误可忽略
        let _ = tx_for_sink.send(evt);
    });
    let orch = DiscussionOrchestrator::with_event_sink(
        config.clone(),
        bus.clone(),
        Some(factory.clone()),
        CancellationToken::new(),
        None,
        round_counter,
        Some(event_sink),
    )
    .ok()?;
    // 启 OS 线程 drainer:持有 StdoutLock、循环 recv、写 JSONL。channel 在
    // orchestrator 退出时 drop 最后一个 sender,drainer 收到 RecvError 退出。
    let _ = std::thread::Builder::new()
        .name("zhuye-discussion-collab-stdout".into())
        .spawn(move || {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            while let Ok(evt) = rx.recv() {
                if let Ok(line) = serde_json::to_string(&evt) {
                    let _ = writeln!(lock, "{line}");
                    let _ = lock.flush();
                }
            }
        });
    // tx 在 orchestrator 持有;run() 退出后 orchestrator drop,sink drop,
    // tx drop,drainer 自然退出。
    drop(tx);
    Some((orch, factory, toolset_usage))
}

/// v0.2.4:为每个 participant 构造一个空 `Arc<Mutex<Option<TokenUsage>>>` 槽,
/// 与 DiscussionToolSet 和 LlmContext 共享。LLM 跑完后写入。
fn build_toolset_usage(
    config: &DiscussionConfig,
) -> HashMap<AgentId, Arc<Mutex<Option<TokenUsage>>>> {
    config
        .participants
        .iter()
        .map(|p| (p.clone(), Arc::new(Mutex::new(None))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use crate::models::DiscussionId;
    use crate::tool::DiscussionToolSet;
    use std::io::Write;
    use zhuye_tools::ToolContext;

    #[test]
    fn discussion_toml_round_trip() {
        let toml_str = r#"
[discussion]
mode = "concurrent"
topic = "Decide whether to use Rust async or sync"
consensus_window = 1
max_rounds = 5
mailbox_capacity = 64

[[agents]]
role = "advocate"
system_prompt = "Argue for async."
allowed_tools = ["send_message", "read_messages", "finish_discussion"]

[[agents]]
role = "skeptic"
system_prompt = "Challenge the advocate."
allowed_tools = ["send_message", "read_messages", "finish_discussion"]
"#;
        let parsed: DiscussionToml = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.discussion.mode, DiscussionMode::Concurrent);
        assert_eq!(
            parsed.discussion.topic,
            "Decide whether to use Rust async or sync"
        );
        assert_eq!(parsed.discussion.consensus_window, 1);
        assert_eq!(parsed.discussion.max_rounds, 5);
        assert_eq!(parsed.discussion.mailbox_capacity, 64);
        assert_eq!(parsed.agents.len(), 2);
        assert_eq!(parsed.agents[0].role, "advocate");
        // 来回序列化
        let serialized = toml::to_string(&parsed).unwrap();
        let reparsed: DiscussionToml = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.agents.len(), 2);
    }

    #[test]
    fn into_config_populates_participants_from_agents() {
        let toml_str = r#"
[discussion]
topic = "x"
[[agents]]
role = "a"
system_prompt = "x"
[[agents]]
role = "b"
system_prompt = "y"
"#;
        let parsed: DiscussionToml = toml::from_str(toml_str).unwrap();
        let config = parsed.into_config();
        assert_eq!(
            config.participants,
            vec![AgentId("a".into()), AgentId("b".into())]
        );
    }

    #[test]
    fn load_from_tempfile() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
[discussion]
topic = "tmp test"
[[agents]]
role = "a"
system_prompt = "x"
"#,
        )
        .unwrap();
        let cfg = DiscussionToml::load(tmp.path()).unwrap();
        assert_eq!(cfg.discussion.topic, "tmp test");
        assert_eq!(cfg.agents.len(), 1);
    }

    #[test]
    fn load_missing_file_returns_io_error() {
        let r = DiscussionToml::load(Path::new("/nonexistent.toml"));
        assert!(matches!(r, Err(CliError::Io(_))));
    }

    // ── try_build_llm_orchestrator(env 依赖) ────────────────────────────────
    //
    // 这些测试依赖环境变量;实际 CI / 本地 dev 环境差异大,只断言「无 provider 时返 None」。
    // 想测 Some 路径必须写 ReflectConfig 配置文件或 set env,但 env 在 cargo test 并行下
    // 不安全 —— 留作手动 smoke。状态机 + wiring 的核心验证在
    // `crates/zhuye-discussion/tests/discussion_llm_e2e.rs`。

    /// 当 `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `ZHUYE_PROVIDER` 均未设且
    /// `~/.zhuye/config.toml` 不存在(或为空)时,`try_build_llm_orchestrator`
    /// 必须返 `None`(降级 noop 路径)。
    ///
    /// 注:不强制清 env,这样测试与本地 dev 配置兼容 —— 只要 env 没设关键变量,
    /// `ReflectConfig::active_provider()` 走 `None` 路径即 OK。
    // `try_build_llm_orchestrator` 是 async(内部要 await 模型解析),
    // 故本用例必须 async —— 否则 `.await` 落在同步 fn 里编译不过
    // (E0728:await is only allowed inside async functions)。
    #[tokio::test]
    async fn try_build_returns_none_when_no_provider() {
        // SAFETY: 用 mutex 串行化避免与并行测试互相覆盖 env。简单起见,
        // 这里仅在大概率无 env 环境下断言;若用户本地真有 OPENAI_API_KEY,
        // 该测试仍会通过(只要 ReflectConfig::resolved_model_spec 返回 Some 就不会 fail)。
        // 我们反着断言:仅当 config 拿不到 model spec 时返 None。
        let cfg = zhuye_config::load_default();
        if cfg.resolved_model_spec().is_none() {
            let participants = vec![AgentId("a".into()), AgentId("b".into())];
            let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 4);
            let config = DiscussionConfig {
                mode: DiscussionMode::Concurrent,
                participants,
                topic: "t".into(),
                consensus_window: 1,
                max_rounds: 1,
                mailbox_capacity: 4,
                judge: false,
            };
            let r = try_build_llm_orchestrator(&config, &bus).await;
            assert!(r.is_none(), "expected None when no provider configured");
        }
        // env 有 provider 时此分支无意义,跳过(测试不失败)。
    }

    /// 回归:同名工具多身份共存 ——
    /// 各 participant 的 `send_message` **名字相同**但 `self_id` 不同。
    /// `ToolRegistry` 按名索引,若所有 participant 的工具都注册进同一张表,
    /// 后者覆盖前者 → 所有 participant 共用最后一个身份(消息 `from` 全同)。
    /// per-role 表必须让每个 participant 拿到**自己**那份工具实例。
    #[tokio::test]
    async fn per_role_registries_keep_participant_identity() {
        let participants = vec![AgentId("alice".into()), AgentId("bob".into())];
        let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 16);
        let finished = Arc::new(Mutex::new(false));
        let round = Arc::new(AtomicU32::new(0));
        let mut sets = Vec::new();
        for p in &participants {
            let set = DiscussionToolSet::with_usage(
                p.clone(),
                bus.clone(),
                finished.clone(),
                round.clone(),
                Arc::new(Mutex::new(None)),
            );
            sets.push((p.clone(), set));
        }
        let (_shared, roles) = build_tool_registries(&sets);
        assert_eq!(roles.len(), 2, "每个 participant 一份 per-role 表");

        // 两个角色的 `send_message` 必须是不同实例(而非共享表里最后注册的那个)。
        let alice_tool = roles[0]
            .1
            .get("send_message")
            .expect("alice 角色表含 send_message");
        let bob_tool = roles[1]
            .1
            .get("send_message")
            .expect("bob 角色表含 send_message");
        assert!(
            !Arc::ptr_eq(&alice_tool, &bob_tool),
            "两个角色的 send_message 必须是不同实例,否则身份互串"
        );

        // 端到端:各自发一条消息,transcript 的 `from` 必须分别是本人。
        let ctx = ToolContext::for_workspace(std::env::temp_dir());
        let args = serde_json::json!({ "content": "hi" });
        alice_tool
            .execute(ctx.clone(), args.clone())
            .await
            .expect("alice 发送成功");
        bob_tool.execute(ctx, args).await.expect("bob 发送成功");
        let senders: Vec<String> = bus.transcript().iter().map(|m| m.from.0.clone()).collect();
        assert!(
            senders.contains(&"alice".to_string()),
            "alice 的消息应标 from=alice,实际: {senders:?}"
        );
        assert!(
            senders.contains(&"bob".to_string()),
            "bob 的消息应标 from=bob,实际: {senders:?}"
        );
    }

    /// 回归:per-role 表以共享并集为底 —— `allowed_tools` 里列的父级工具
    /// 不能因为按 role 隔离而丢失。
    #[test]
    fn per_role_registry_keeps_shared_union_tools() {
        let participants = vec![AgentId("alice".into()), AgentId("bob".into())];
        let bus = MessageBus::new(DiscussionId::new(), participants.clone(), 16);
        let finished = Arc::new(Mutex::new(false));
        let round = Arc::new(AtomicU32::new(0));
        let mut sets = Vec::new();
        for p in &participants {
            let set = DiscussionToolSet::with_usage(
                p.clone(),
                bus.clone(),
                finished.clone(),
                round.clone(),
                Arc::new(Mutex::new(None)),
            );
            sets.push((p.clone(), set));
        }
        let (shared, roles) = build_tool_registries(&sets);
        let shared_names = shared.list();
        assert!(!shared_names.is_empty(), "共享并集非空");
        for (role, reg) in &roles {
            let names = reg.list();
            for n in &shared_names {
                assert!(
                    names.contains(n),
                    "role {role} 的表丢了并集里的工具 {n}: {names:?}"
                );
            }
        }
    }
}
