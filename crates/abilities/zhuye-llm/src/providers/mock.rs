//! `MockClient` —— 内置 mock provider(离线测试 / SDK 冒烟用)。
//!
//! 注册条件由 `zhuye-config` builder 决定:`ZHUYE_PROVIDER=mock`,或
//! `ZHUYE_MODEL` 形如 `mock` / `mock/...` 时,把本 client 注册为
//! `"mock"` pool(免 API key、零网络)。
//!
//! 行为:
//! - env `ZHUYE_MOCK_SCRIPT` 指向一个 JSONL 文件,**每行对应一次模型
//!   调用**(一次 `stream()`),按顺序消耗:
//!   - `{"type":"text","text":"..."}` —— 回复一段文本(拆成两个 delta,
//!     锻炼下游的流式聚合路径);
//!   - `{"type":"tool_call","name":"...","args":{...}}` —— 发起一次
//!     工具调用(engine 执行工具后会再次调用模型,即再消耗一行));
//!   - `{"type":"tool_call_delta","name":"...","chunks":["...","..."]}` ——
//!     同上,但参数**分片**下发(分片形态 provider:Anthropic / OpenAI 的
//!     「start → 一/多个 delta → stop」)。engine 侧逐段转发
//!     `tool_call_args_delta` 事件,供客户端渐进渲染工具入参。
//! - 脚本耗尽或未配置时,回退固定回复(文案自带「mock 固定回复」标记,
//!   与真实回复可区分,离线测试不再假绿);
//! - 脚本路径不存在 / 读取失败 / 非空文件但全部行解析失败时,
//!   首次模型调用显式返回 `InvalidRequest`(配置错误沿
//!   `EventMsg::Error` 通路冒泡,不静默降级)。
//!
//! 这让「turn 1 调工具、turn 2 收尾文本」之类的多阶段对话可以用一个
//! 两行脚本确定性编排,是 TS / Python SDK 集成测试的基础设施。

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use futures::stream;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::capabilities::Capabilities;
use crate::client::ModelClient;
use crate::error::LlmError;
use crate::event::ChatEvent;
use crate::request::ChatRequest;

/// env 变量:mock 脚本(JSONL 文件路径)。
pub const ENV_MOCK_SCRIPT: &str = "ZHUYE_MOCK_SCRIPT";

/// 脚本耗尽 / 未配置时的固定回复。
///
/// 文案自带「这是 mock 固定回复」的可辨识标记 ——
/// 此前裸的 `"Hello from mock LLM!"` 与真实回复无法区分,脚本写错 /
/// 覆盖不足时被测流程若只断言文本非空,测试会假通过。
pub const DEFAULT_MOCK_REPLY: &str =
    "Hello from mock LLM! (zhuye mock 固定回复:ZHUYE_MOCK_SCRIPT 未配置或已耗尽)";

/// mock 默认模型名(spec 形如 `mock/mock-1`)。
pub const DEFAULT_MOCK_MODEL: &str = "mock-1";

/// 单次模型调用的脚本化回复。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MockReply {
    /// 回复一段文本。
    Text { text: String },
    /// 发起一次工具调用。
    ToolCall {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// 发起一次工具调用,参数**分片**下发(分片形态 provider:Anthropic /
    /// OpenAI 逐段 emit 参数 JSON)。
    ///
    /// 与 `ToolCall` 的差别:`ToolUseStart` **不带**参数,参数由若干
    /// `ToolUseDelta` 分片送来 —— engine 侧走 `args_from_start == false`
    /// 分支,逐段转发 `tool_call_args_delta` 事件给客户端。
    ToolCallDelta {
        name: String,
        /// 参数 JSON 的分片(按序拼接成完整 JSON)。
        #[serde(default)]
        chunks: Vec<String>,
    },
    /// 回显引擎请求的观测快照(tools 名单 + system 各块文本)——
    /// 能力覆盖层用它对「引擎实际发给模型的 system prompt / 工具集」
    /// 做字段级断言(离线、免探针)。回复文本 = 一段 JSON:
    /// `{"tools":["read","bash",...],"system":["<block text>",...]}`。
    RequestEcho,
}

/// 内置 mock client。脚本队列在所有调用间共享(每次 `stream()` 消耗一行)。
pub struct MockClient {
    script: Mutex<VecDeque<MockReply>>,
    seq: AtomicU32,
    /// 已收到的请求快照(按调用顺序追加)—— 测试断言「模型实际看到什么」
    /// 用,比如插件 slash 命令展开后的 prompt 文本。
    requests: Mutex<Vec<ChatRequest>>,
    /// 脚本配置错误(路径不存在 / 读取失败 / 非空文件但全部
    /// 行解析失败)。`Some` 时每次 `stream()` 直接返回
    /// `LlmError::InvalidRequest` —— 配置错误沿真实错误通路冒泡为
    /// `EventMsg::Error`,不再静默回退固定回复(那会让离线测试假绿)。
    config_error: Option<String>,
}

impl MockClient {
    /// 空脚本构造(每次调用回退默认回复)。
    pub fn new() -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            seq: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
            config_error: None,
        }
    }

    /// 从 env `ZHUYE_MOCK_SCRIPT` 加载脚本构造。
    ///
    /// 文件不存在 / 读取失败 / 非空内容但零条有效行时,错误
    /// 记入 `config_error`(`stream()` 时以 `InvalidRequest` 抛出,让
    /// engine → `EventMsg::Error` → SDK 异常的通路显式失败),同时启动期
    /// 保留 `tracing::warn`。个别行解析失败但仍存在有效行时维持旧行为
    /// (warn + 跳过该行),不阻塞可用脚本。
    pub fn from_env() -> Self {
        let mut client = Self::new();
        let Ok(path) = std::env::var(ENV_MOCK_SCRIPT) else {
            return client;
        };
        if path.is_empty() {
            return client;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut script = client.script.lock();
                let mut parse_failures = 0usize;
                for (idx, line) in content.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<MockReply>(line) {
                        Ok(reply) => script.push_back(reply),
                        Err(e) => {
                            parse_failures += 1;
                            tracing::warn!(
                                file = %path,
                                line = idx + 1,
                                error = %e,
                                "mock 脚本行解析失败,跳过该行"
                            );
                        }
                    }
                }
                // 非空文件却零条有效行:几乎必然是字段名/格式写错
                // (如 `tool`/`arguments` 而非 `type`/`name`/`args`)。
                // 记入 config_error,首次模型调用即显式报错。
                if script.is_empty() && parse_failures > 0 {
                    client.config_error = Some(format!(
                        "ZHUYE_MOCK_SCRIPT 解析失败:文件 {path} 共 {parse_failures} 行\
                         全部无法解析(期望 JSONL,每行形如 {{\"type\":\"text\",\"text\":...}} / \
                         {{\"type\":\"tool_call\",\"name\":...,\"args\":{{...}}}} / \
                         {{\"type\":\"tool_call_delta\",\"name\":...,\"chunks\":[...]}})"
                    ));
                    tracing::warn!(file = %path, failures = parse_failures, "mock 脚本全部行解析失败;模型调用将显式报错");
                } else {
                    tracing::debug!(file = %path, lines = script.len(), "mock 脚本已加载");
                }
            }
            Err(e) => {
                client.config_error = Some(format!(
                    "ZHUYE_MOCK_SCRIPT 指向的文件读取失败:{path}: {e}(期望 JSONL 文件路径,\
                     而非内联脚本内容)"
                ));
                tracing::warn!(file = %path, error = %e, "mock 脚本读取失败;模型调用将显式报错");
            }
        }
        client
    }

    /// 测试辅助:直接给定脚本队列。
    pub fn with_script(script: Vec<MockReply>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            seq: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
            config_error: None,
        }
    }

    /// 测试辅助:构造带配置错误的 client(模拟脚本文件损坏)。
    pub fn with_config_error(message: String) -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            seq: AtomicU32::new(0),
            requests: Mutex::new(Vec::new()),
            config_error: Some(message),
        }
    }

    /// 已收到的请求快照(按调用顺序)。测试用它断言引擎发给模型的
    /// 消息内容(如命令展开后的用户文本)。
    pub fn recorded_requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().clone()
    }
}

impl Default for MockClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelClient for MockClient {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        // 声明支持 tool_use,让 engine 把工具 schema 发给 mock ——
        // 脚本化的 tool_call 回复才能走真实的工具执行回路。
        Capabilities {
            tool_use: true,
            ..Default::default()
        }
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
    {
        // 记录请求快照(测试可观测性;失败不影响主流程)。
        self.requests.lock().push(request.clone());
        // 配置错误(脚本读不到 / 全部行解析失败)显式报错,
        // 走 InvalidRequest(GiveUp 路径)→ engine 发 EventMsg::Error。
        if let Some(err) = &self.config_error {
            return Err(LlmError::InvalidRequest {
                message: err.clone(),
            });
        }
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let reply = self.script.lock().pop_front();

        let mut chunks: Vec<Result<ChatEvent, LlmError>> = vec![Ok(ChatEvent::MessageStart {
            id: format!("msg_mock_{n}"),
            model: DEFAULT_MOCK_MODEL.to_string(),
        })];

        match reply {
            Some(MockReply::Text { text }) => push_text_deltas(&mut chunks, text),
            Some(MockReply::ToolCall { name, args }) => {
                // mock 一次性把 args 塞进单个 delta,模拟 provider 完整参数
                // 到达的形态(Anthropic / OpenAI 都按「start → 一/多个 delta
                // → message_stop」模式 emit)。
                //
                // 同时**也**把它放进 ToolUseStart.input_json —— 那是
                // 「整包形态」的 provider(Ollama 原生格式)的做法:参数
                // 全在 start 里、没有 delta。engine 两种形态都要认
                // (见 zhuye-core 的 `args_from_start`),所以这里两种都发,
                // 顺带当 engine 侧的回归夹具。
                let input_json = if args.is_null() {
                    String::new()
                } else {
                    args.to_string()
                };
                chunks.push(Ok(ChatEvent::ToolUseStart {
                    id: format!("toolu_mock_{n}"),
                    name,
                    input_json: input_json.clone(),
                }));
                if !input_json.is_empty() {
                    chunks.push(Ok(ChatEvent::ToolUseDelta(input_json)));
                }
            }
            Some(MockReply::ToolCallDelta {
                name,
                chunks: parts,
            }) => {
                // 分片形态:`ToolUseStart` 不带参数(留空),参数由若干
                // `ToolUseDelta` 逐段送达 —— 与 Anthropic / OpenAI 的
                // 「start → 一/多个 delta → stop」一致。engine 侧据此
                // 逐段转发 `tool_call_args_delta` 给客户端。
                chunks.push(Ok(ChatEvent::ToolUseStart {
                    id: format!("toolu_mock_{n}"),
                    name,
                    input_json: String::new(),
                }));
                for part in parts {
                    chunks.push(Ok(ChatEvent::ToolUseDelta(part)));
                }
            }
            Some(MockReply::RequestEcho) => {
                // 观测快照:把引擎发给本 provider 的 tools 名单与 system
                // 各块文本序列化成 JSON 作为回复文本,供上层断言。
                let snapshot = serde_json::json!({
                    "tools": request
                        .tools
                        .iter()
                        .map(|t| t.name().to_string())
                        .collect::<Vec<_>>(),
                    "system": request
                        .system
                        .0
                        .iter()
                        .map(|b| b.text.clone())
                        .collect::<Vec<_>>(),
                });
                push_text_deltas(&mut chunks, snapshot.to_string());
            }
            None => push_text_deltas(&mut chunks, DEFAULT_MOCK_REPLY.to_string()),
        }

        chunks.push(Ok(ChatEvent::Usage {
            input_tokens: 0,
            output_tokens: 4,
            cached_tokens: 0,
            cache_write_tokens: 0,
        }));
        chunks.push(Ok(ChatEvent::MessageStop));

        Ok(Box::pin(stream::iter(chunks)))
    }
}

/// 把一段文本拆成两个 ContentDelta 推入事件列表(单字符 / 空文本退化为
/// 单个 delta)。拆分是为了验证下游的流式 delta 聚合路径。
fn push_text_deltas(chunks: &mut Vec<Result<ChatEvent, LlmError>>, text: String) {
    let mid = text.char_indices().count() / 2;
    match text.char_indices().nth(mid).map(|(i, _)| i) {
        Some(i) => {
            chunks.push(Ok(ChatEvent::ContentDelta(text[..i].to_string())));
            chunks.push(Ok(ChatEvent::ContentDelta(text[i..].to_string())));
        }
        None => chunks.push(Ok(ChatEvent::ContentDelta(text))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// 收集一次 stream 的全部事件(测试辅助)。
    async fn collect(client: &MockClient) -> Vec<ChatEvent> {
        let stream = client
            .stream(ChatRequest::default(), CancellationToken::new())
            .await
            .expect("mock stream 不应失败");
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev.expect("mock 事件不应出错"));
        }
        out
    }

    #[tokio::test]
    async fn 默认回复拆成两个_delta() {
        let client = MockClient::new();
        let events = collect(&client).await;
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ContentDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas.concat(), DEFAULT_MOCK_REPLY.to_string());
        // 末尾必须有 MessageStop,否则 engine 不会结束回合。
        assert!(matches!(events.last(), Some(ChatEvent::MessageStop)));
    }

    #[tokio::test]
    async fn 脚本文本回复按顺序消耗() {
        let client = MockClient::with_script(vec![
            MockReply::Text {
                text: "第一轮".into(),
            },
            MockReply::Text {
                text: "第二轮".into(),
            },
        ]);
        let first = collect(&client).await;
        let second = collect(&client).await;
        let text_of = |evs: &[ChatEvent]| {
            evs.iter()
                .filter_map(|e| match e {
                    ChatEvent::ContentDelta(d) => Some(d.clone()),
                    _ => None,
                })
                .collect::<String>()
        };
        assert_eq!(text_of(&first), "第一轮");
        assert_eq!(text_of(&second), "第二轮");
        // 耗尽后回退默认回复。
        let third = collect(&client).await;
        assert!(text_of(&third).contains(DEFAULT_MOCK_REPLY));
    }

    #[tokio::test]
    async fn 脚本工具调用产生_tool_use_start() {
        let client = MockClient::with_script(vec![MockReply::ToolCall {
            name: "get_weather".into(),
            args: serde_json::json!({"city": "北京"}),
        }]);
        let events = collect(&client).await;
        let tool = events.iter().find_map(|e| match e {
            ChatEvent::ToolUseStart {
                name, input_json, ..
            } => Some((name.clone(), input_json.clone())),
            _ => None,
        });
        let (name, input_json) = tool.expect("应有 ToolUseStart 事件");
        assert_eq!(name, "get_weather");
        assert_eq!(input_json, r#"{"city":"北京"}"#);
    }

    #[test]
    fn 脚本行_serde_解析() {
        let text: MockReply = serde_json::from_str(r#"{"type":"text","text":"hi"}"#).unwrap();
        assert_eq!(text, MockReply::Text { text: "hi".into() });
        let call: MockReply =
            serde_json::from_str(r#"{"type":"tool_call","name":"echo","args":{"a":1}}"#).unwrap();
        assert_eq!(
            call,
            MockReply::ToolCall {
                name: "echo".into(),
                args: serde_json::json!({"a": 1})
            }
        );
        // 分片形态:参数按 chunks 逐段下发。
        let delta: MockReply = serde_json::from_str(
            r#"{"type":"tool_call_delta","name":"echo","chunks":["{\"a\":1","}"]}"#,
        )
        .unwrap();
        assert_eq!(
            delta,
            MockReply::ToolCallDelta {
                name: "echo".into(),
                chunks: vec!["{\"a\":1".into(), "}".into()],
            }
        );
        // args 缺省为 null。
        let no_args: MockReply =
            serde_json::from_str(r#"{"type":"tool_call","name":"echo"}"#).unwrap();
        assert_eq!(
            no_args,
            MockReply::ToolCall {
                name: "echo".into(),
                args: serde_json::Value::Null
            }
        );
        // 观测快照:无字段的裸标记即可解析。
        let echo: MockReply = serde_json::from_str(r#"{"type":"request_echo"}"#).unwrap();
        assert_eq!(echo, MockReply::RequestEcho);
    }

    #[test]
    fn 消息_id_单调递增() {
        let client = MockClient::new();
        assert_eq!(client.seq.load(Ordering::Relaxed), 0);
    }

    /// 配置错误时 `stream()` 必须显式报错(InvalidRequest,
    /// GiveUp 路径),不再静默回退固定回复。
    #[tokio::test]
    async fn 配置错误时_stream_显式报错() {
        let client = MockClient::with_config_error("脚本读取失败:/tmp/nope".into());
        let Err(e) = client
            .stream(ChatRequest::default(), CancellationToken::new())
            .await
        else {
            panic!("配置错误时 stream 必须返回 Err");
        };
        match e {
            LlmError::InvalidRequest { message } => {
                assert!(message.contains("脚本读取失败"), "{message}");
            }
            other => panic!("期望 InvalidRequest 错误,得到 {other:?}"),
        }
    }

    /// 默认回复文案必须自带「mock 固定回复」可辨识标记,
    /// 让「脚本未配置/耗尽」的静默回退在断言文本时能被发现。
    #[tokio::test]
    async fn 默认回复自带mock标记() {
        assert!(DEFAULT_MOCK_REPLY.contains("mock 固定回复"));
        let client = MockClient::new();
        let events = collect(&client).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ContentDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, DEFAULT_MOCK_REPLY.to_string());
    }

    /// 非空文件但全部行解析失败 → from_env 记 config_error。
    /// 路径不可读 → 同样记 config_error。两个分支都不再静默降级。
    #[test]
    fn from_env_全部行解析失败时记录配置错误() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();

        // 场景一:字段名写错(tool/arguments)→ 全部行解析失败。
        let bad = dir.path().join("bad.jsonl");
        std::fs::write(&bad, "{\"tool\":\"echo\",\"arguments\":{\"a\":1}}\n").unwrap();
        let orig = std::env::var_os(ENV_MOCK_SCRIPT);
        // SAFETY:已持互斥锁,单线程内无并发读者。
        unsafe {
            std::env::set_var(ENV_MOCK_SCRIPT, &bad);
        }
        let client = MockClient::from_env();
        assert!(
            client.config_error.is_some(),
            "全部行解析失败必须记 config_error"
        );
        let msg = client.config_error.clone().unwrap();
        assert!(msg.contains("全部无法解析"), "{msg}");

        // 场景二:路径不存在 → config_error(此前静默回退默认回复)。
        let missing = dir.path().join("missing.jsonl");
        // SAFETY:同上。
        unsafe {
            std::env::set_var(ENV_MOCK_SCRIPT, &missing);
        }
        let client = MockClient::from_env();
        assert!(
            client.config_error.is_some(),
            "路径不存在必须记 config_error"
        );

        // 还原 env。
        // SAFETY:同上。
        unsafe {
            match orig {
                Some(v) => std::env::set_var(ENV_MOCK_SCRIPT, v),
                None => std::env::remove_var(ENV_MOCK_SCRIPT),
            }
        }
    }
}
