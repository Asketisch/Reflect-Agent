//! `MockClient` —— 内置 mock provider(离线测试 / SDK 冒烟用)。
//!
//! 注册条件由 `reflect-config` builder 决定:`REFLECT_PROVIDER=mock`,或
//! `REFLECT_MODEL` 形如 `mock` / `mock/...` 时,把本 client 注册为
//! `"mock"` pool(免 API key、零网络)。
//!
//! 行为:
//! - env `REFLECT_MOCK_SCRIPT` 指向一个 JSONL 文件,**每行对应一次模型
//!   调用**(一次 `stream()`),按顺序消耗:
//!   - `{"type":"text","text":"..."}` —— 回复一段文本(拆成两个 delta,
//!     锻炼下游的流式聚合路径);
//!   - `{"type":"tool_call","name":"...","args":{...}}` —— 发起一次
//!     工具调用(engine 执行工具后会再次调用模型,即再消耗一行)。
//! - 脚本耗尽或未配置时,回退固定回复 `"Hello from mock LLM!"`。
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
pub const ENV_MOCK_SCRIPT: &str = "REFLECT_MOCK_SCRIPT";

/// 脚本耗尽 / 未配置时的固定回复(与历史 `custom_provider` 示例一致)。
pub const DEFAULT_MOCK_REPLY: &str = "Hello from mock LLM!";

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
}

/// 内置 mock client。脚本队列在所有调用间共享(每次 `stream()` 消耗一行)。
pub struct MockClient {
    script: Mutex<VecDeque<MockReply>>,
    seq: AtomicU32,
}

impl MockClient {
    /// 空脚本构造(每次调用回退默认回复)。
    pub fn new() -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            seq: AtomicU32::new(0),
        }
    }

    /// 从 env `REFLECT_MOCK_SCRIPT` 加载脚本构造。文件不存在 / 解析失败
    /// 时 warn 并回退空脚本(不让配置错误阻塞启动,与 config 层的
    /// best-effort 风格一致)。
    pub fn from_env() -> Self {
        let client = Self::new();
        let Ok(path) = std::env::var(ENV_MOCK_SCRIPT) else {
            return client;
        };
        if path.is_empty() {
            return client;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut script = client.script.lock();
                for (idx, line) in content.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<MockReply>(line) {
                        Ok(reply) => script.push_back(reply),
                        Err(e) => {
                            tracing::warn!(
                                file = %path,
                                line = idx + 1,
                                error = %e,
                                "mock 脚本行解析失败,跳过该行"
                            );
                        }
                    }
                }
                tracing::debug!(file = %path, lines = script.len(), "mock 脚本已加载");
            }
            Err(e) => {
                tracing::warn!(file = %path, error = %e, "mock 脚本读取失败,使用默认回复");
            }
        }
        client
    }

    /// 测试辅助:直接给定脚本队列。
    pub fn with_script(script: Vec<MockReply>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            seq: AtomicU32::new(0),
        }
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
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
    {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let reply = self.script.lock().pop_front();

        let mut chunks: Vec<Result<ChatEvent, LlmError>> = vec![Ok(ChatEvent::MessageStart {
            id: format!("msg_mock_{n}"),
            model: DEFAULT_MOCK_MODEL.to_string(),
        })];

        match reply {
            Some(MockReply::Text { text }) => push_text_deltas(&mut chunks, text),
            Some(MockReply::ToolCall { name, args }) => {
                // engine 在 ToolUseStart 后必须接收 ToolUseDelta 才能
                // 累积 args 并喂给工具;mock 一次性把 args 塞进单个 delta,
                // 模拟 provider 完整参数到达的形态(Anthropic / OpenAI
                // 都按「start → 一/多个 delta → message_stop」模式 emit)。
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
    }

    #[test]
    fn 消息_id_单调递增() {
        let client = MockClient::new();
        assert_eq!(client.seq.load(Ordering::Relaxed), 0);
    }
}
