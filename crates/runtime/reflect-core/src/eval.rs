//! v1.5 E3:数据驱动评估运行器 —— 把全链路测试从手写 Rust 泛化为
//! **场景 JSON**:操作序列 + mock 模型脚本 + 断言,一条 API 跑完出报告。
//!
//! 场景形态(全部 JSON 可序列化,可直接落盘为场景文件):
//!
//! ```json
//! {
//!   "name": "tool-loop-smoke",
//!   "ops": [
//!     { "user_input": { "text": "run echo ping" } }
//!   ],
//!   "script": [ [ { "type": "message_start" } ] ],
//!   "expect": {
//!     "event_order": ["turn_started", "tool_call_begin", "turn_complete"],
//!     "tool_calls": ["echo"],
//!     "request_contains": { "1": ["ping"] },
//!     "final_status": "success"
//!   }
//! }
//! ```
//!
//! 唯一被 mock 的是 LLM(脚本按模型调用次序回放);引擎、工具队列、
//! 事件流全真。断言全部为**子序列 / 包含**语义,对异步事件到达顺序
//! 的抖动稳健。用途:回归评估集(`evals/*.json`)+ CI 冒烟。

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, Submission, UserInputItem};
use reflect_tools::ToolRegistry;

use crate::{AgentConfig, AgentThread};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// 单个场景操作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalOp {
    /// 提交一条用户输入。`wait = true`(默认)时等待本回合终态再继续;
    /// `wait = false` 立即返回(供后续 `interrupt` 打断在飞回合)。
    #[serde(rename = "user_input")]
    UserInput {
        text: String,
        #[serde(default = "default_true")]
        wait: bool,
    },
    /// 回合中途转向(Now 优先级):无等待,攒到下一个回合边界合并。
    #[serde(rename = "steer")]
    Steer { text: String },
    /// 中断当前在飞回合(等待 TurnAborted)。
    #[serde(rename = "interrupt")]
    Interrupt,
}

fn default_true() -> bool {
    true
}

/// 断言集合(全部可选;子序列 / 包含语义)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalExpectations {
    /// 事件判别名须按此子序列出现。
    #[serde(default)]
    pub event_order: Vec<String>,
    /// 期望的工具调用名序列(按发起顺序,完整匹配)。
    #[serde(default)]
    pub tool_calls: Vec<String>,
    /// 第 N 次模型请求的**消息文本**(含 user / assistant / tool 结果)
    /// 须包含的片段(key = 调用序号,0 起)。
    #[serde(default)]
    pub request_contains: HashMap<String, Vec<String>>,
    /// 终态:`success` / `aborted`(默认 success;多回合场景看**最后**
    /// 一个终态)。
    #[serde(default)]
    pub final_status: FinalStatus,
    /// 期望的模型调用次数(可选)。
    #[serde(default)]
    pub model_calls: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalStatus {
    #[default]
    Success,
    Aborted,
}

/// v1.5 E3:脚本步骤 —— 对手写场景友好的模型回放中间形态。
///
/// 运行时映射为 `ChatEvent` 流(自动补 `MessageStart` / `MessageStop`
/// 首尾;`tool_use` 的 args 自动序列化为单个 delta,与真实 provider
/// 的「start → delta → stop」模式一致)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptStep {
    /// 文本增量(一段或多段,按序拼接)。
    Text { text: String },
    /// 思考增量(extended thinking)。
    Thinking { text: String },
    /// 发起一次工具调用(参数对象自动序列化为增量 JSON)。
    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// 用量快照(TokenCount 聚合 / 耗尽判定的数据源)。
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        #[serde(default)]
        cached_tokens: u32,
        #[serde(default)]
        cache_write_tokens: u32,
    },
    /// 以「输出触顶」终止(引擎自动续作)。
    Truncated { stop_reason: String },
}

/// 评估场景(名称 + 操作 + 脚本 + 断言)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalScenario {
    pub name: String,
    #[serde(default)]
    pub ops: Vec<EvalOp>,
    /// mock 模型脚本:第 i 个元素 = 第 i 次模型调用的步骤序列
    /// (见 [`ScriptStep`] —— 对手写场景友好的中间形态,运行时映射为
    /// `ChatEvent` 流)。
    #[serde(default)]
    pub script: Vec<Vec<ScriptStep>>,
    #[serde(default)]
    pub expect: EvalExpectations,
}

impl EvalScenario {
    /// 从 JSON 文件加载场景。
    pub fn from_json_file(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// 从 JSON 字符串加载。
    pub fn from_json_str(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("parse scenario: {e}"))
    }
}

/// 评估结果。
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub name: String,
    pub passed: bool,
    /// 每条失败断言的描述(通过时为空)。
    pub failures: Vec<String>,
    pub model_calls: usize,
    pub duration_ms: u64,
}

/// 内部:mock 模型(按调用次序回放 + 请求捕获)。
struct ScriptedClient {
    scripts: std::sync::Mutex<Vec<Vec<ScriptStep>>>,
    requests: std::sync::Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ModelClient for ScriptedClient {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().unwrap().push(request);
        let steps = {
            let mut q = self.scripts.lock().unwrap();
            if q.is_empty() {
                Vec::new() // 脚本耗尽兜底:仅首尾事件,保证收敛。
            } else {
                q.remove(0)
            }
        };
        // ScriptStep → ChatEvent 流(自动补 MessageStart / MessageStop)。
        let mut events: Vec<ChatEvent> = vec![ChatEvent::MessageStart {
            id: "m".into(),
            model: "scripted".into(),
        }];
        for step in steps {
            match step {
                ScriptStep::Text { text } => events.push(ChatEvent::ContentDelta(text)),
                ScriptStep::Thinking { text } => events.push(ChatEvent::ThinkingDelta(text)),
                ScriptStep::ToolUse { id, name, args } => {
                    events.push(ChatEvent::ToolUseStart {
                        id,
                        name,
                        input_json: String::new(),
                    });
                    events.push(ChatEvent::ToolUseDelta(args.to_string()));
                }
                ScriptStep::Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens,
                } => events.push(ChatEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens,
                }),
                ScriptStep::Truncated { stop_reason } => {
                    events.push(ChatEvent::MessageStopTruncated { stop_reason })
                }
            }
        }
        events.push(ChatEvent::MessageStop);
        Ok(Box::pin(stream::iter(
            events.into_iter().map(Ok::<ChatEvent, LlmError>),
        )))
    }
}

/// 运行一个场景:脚本化 LLM + 真实引擎(echo 工具),返回报告。
/// 不落盘、不触碰网络 —— 可在 CI 任意并发运行。
pub async fn run_scenario(scenario: &EvalScenario, workspace: &Path) -> EvalReport {
    let started = std::time::Instant::now();
    let mut failures: Vec<String> = Vec::new();

    let client = Arc::new(ScriptedClient {
        scripts: std::sync::Mutex::new(scenario.script.clone()),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "scripted",
        CredentialPool {
            entries: vec![PoolEntry {
                client: client.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(reflect_tools::builtins::EchoTool));
    let cfg = AgentConfig::new("scripted/m1", workspace);
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    // ── 顺序执行操作;事件收集到 all_events 供断言 ──
    let deadline = Duration::from_secs(20);
    let mut all_events: Vec<EventMsg> = Vec::new();
    let mut pending_steer: Option<String> = None;
    // 最近一个「未等待」回合的句柄(Interrupt 的中止事件落到它上面)。
    let mut live: Option<crate::TurnHandle> = None;

    for op in &scenario.ops {
        match op {
            EvalOp::UserInput { text, wait } => {
                // 攒下的转向在本回合边界合并(先于用户输入提交)。
                if let Some(st) = pending_steer.take() {
                    let _ = thread.submit(make_steer_sub(&st)).await;
                }
                let mut handle = thread.submit(make_user_sub(text)).await;
                if *wait {
                    collect_until_terminal(&mut handle, &mut all_events, deadline, &mut failures)
                        .await;
                } else {
                    live = Some(handle);
                }
            }
            EvalOp::Steer { text } => {
                pending_steer = Some(text.clone());
            }
            EvalOp::Interrupt => {
                let _ = thread
                    .submit(make_sub(Op::Interrupt { child_id: None }))
                    .await;
                // 在飞回合的中止事件在原句柄上;通道随清理关闭即停。
                if let Some(handle) = live.as_mut() {
                    while let Ok(Some(ev)) = timeout(deadline, handle.next()).await {
                        let terminal =
                            matches!(ev.msg, EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_));
                        all_events.push(ev.msg);
                        if terminal {
                            break;
                        }
                    }
                }
                live = None;
            }
        }
    }
    // 攒下未消费的转向:开一个收尾回合消费它。
    if let Some(st) = pending_steer.take() {
        let _ = thread.submit(make_steer_sub(&st)).await;
        let mut handle = thread.submit(make_user_sub("(收尾)")).await;
        collect_until_terminal(&mut handle, &mut all_events, deadline, &mut failures).await;
    }
    // 兜底排空未等待的在飞回合。
    if let Some(handle) = live.as_mut() {
        collect_until_terminal(handle, &mut all_events, deadline, &mut failures).await;
    }

    let requests = client.requests.lock().unwrap().clone();
    let model_calls = requests.len();

    // ── 断言 ──
    // 1. 事件判别名子序列。
    let kinds: Vec<&str> = all_events.iter().map(|e| e.discriminant()).collect();
    let mut pos = 0usize;
    for want in &scenario.expect.event_order {
        while pos < kinds.len() && kinds[pos] != want.as_str() {
            pos += 1;
        }
        if pos >= kinds.len() {
            failures.push(format!("event_order:未按序找到 '{want}'(已见 {kinds:?})"));
            pos = kinds.len();
        } else {
            pos += 1;
        }
    }
    // 2. 工具调用序列(完整匹配)。
    let tools_called: Vec<String> = all_events
        .iter()
        .filter_map(|e| match e {
            EventMsg::ToolCallBegin(b) => Some(b.tool_name.clone()),
            _ => None,
        })
        .collect();
    if tools_called != scenario.expect.tool_calls {
        failures.push(format!(
            "tool_calls:期望 {:?},实际 {:?}",
            scenario.expect.tool_calls, tools_called
        ));
    }
    // 3. 请求包含。
    for (idx_s, needles) in &scenario.expect.request_contains {
        let Ok(idx) = idx_s.parse::<usize>() else {
            failures.push(format!("request_contains:非法序号 '{idx_s}'"));
            continue;
        };
        let Some(req) = requests.get(idx) else {
            failures.push(format!(
                "request_contains:模型调用 {idx} 不存在(共 {model_calls})"
            ));
            continue;
        };
        // 扫描请求的**全部**消息文本(user / assistant / tool)——
        // 典型断言「工具结果已回流进上下文」即依赖 tool 消息。
        let all_text: String = req
            .messages
            .iter()
            .flat_map(|m| match m {
                reflect_llm::ChatMessage::User(u) => u.blocks.iter().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .filter_map(|b| match b {
                reflect_llm::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .chain(req.messages.iter().filter_map(|m| match m {
                reflect_llm::ChatMessage::Tool(t) => Some(t.content_as_text()),
                reflect_llm::ChatMessage::Assistant(a) => a.text.clone(),
                _ => None,
            }))
            .collect::<String>();
        for needle in needles {
            if !all_text.contains(needle.as_str()) {
                failures.push(format!(
                    "request_contains[{idx}]:未包含 '{needle}'(消息文本: {})",
                    truncate(&all_text, 300)
                ));
            }
        }
    }
    // 4. 终态(最后一个)。
    let last_terminal = all_events
        .iter()
        .rev()
        .find(|e| matches!(e, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)));
    match (&scenario.expect.final_status, last_terminal) {
        (FinalStatus::Success, Some(EventMsg::TurnComplete(_))) => {}
        (FinalStatus::Aborted, Some(EventMsg::TurnAborted(_))) => {}
        (FinalStatus::Success, other) => {
            failures.push(format!("final_status=success 但终态为 {other:?}"))
        }
        (FinalStatus::Aborted, other) => {
            failures.push(format!("final_status=aborted 但终态为 {other:?}"))
        }
    }
    // 5. 模型调用次数。
    if let Some(want) = scenario.expect.model_calls {
        if model_calls != want {
            failures.push(format!("model_calls:期望 {want},实际 {model_calls}"));
        }
    }

    EvalReport {
        name: scenario.name.clone(),
        passed: failures.is_empty(),
        failures,
        model_calls,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// 批量运行场景集,逐个产出报告(调用方自行汇总 / 展示)。
pub async fn run_all(scenarios: &[EvalScenario], workspace: &Path) -> Vec<EvalReport> {
    let mut out = Vec::with_capacity(scenarios.len());
    for s in scenarios {
        out.push(run_scenario(s, workspace).await);
    }
    out
}

// ── 内部辅助 ────────────────────────────────────────────────────────

fn make_sub(op: Op) -> Submission {
    Submission {
        id: format!("eval-{}", uuid_like()),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

fn make_user_sub(text: &str) -> Submission {
    make_sub(Op::UserInput {
        items: vec![UserInputItem::Text { text: text.into() }],
        thread_settings: Default::default(),
    })
}

fn make_steer_sub(text: &str) -> Submission {
    make_sub(Op::Steer {
        priority: reflect_protocol::SteeringPriorityMirror::Now,
        items: vec![UserInputItem::Text { text: text.into() }],
    })
}

/// 排空一个回合句柄直到终态(事件进 out;超时记为失败)。
async fn collect_until_terminal(
    handle: &mut crate::TurnHandle,
    out: &mut Vec<EventMsg>,
    deadline: Duration,
    failures: &mut Vec<String>,
) {
    loop {
        match timeout(deadline, handle.next()).await {
            Ok(Some(ev)) => {
                let terminal =
                    matches!(ev.msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_));
                out.push(ev.msg);
                if terminal {
                    return;
                }
            }
            // 通道关闭(回合清理)或超时:记失败并停,防挂死。
            Ok(None) => {
                failures.push("turn channel closed before terminal event".into());
                return;
            }
            Err(_) => {
                failures.push("timeout waiting for terminal event".into());
                return;
            }
        }
    }
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        s.to_string()
    } else {
        let mut cut = cap;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    format!("{:x}", C.fetch_add(1, Ordering::Relaxed))
}
