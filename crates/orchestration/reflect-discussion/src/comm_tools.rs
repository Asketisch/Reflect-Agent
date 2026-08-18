//! `comm_tools` — 讨论通信工具(LLM 可见的三个 Tool)。
//!
//! 这三个工具是 LLM 在讨论中"对外说话/听别人说话/结束讨论"的唯一手段:
//! - [`SendMessageTool`] — 发送一条消息(可广播或单播)
//! - [`ReadMessagesTool`] — 从自己 mailbox 排空所有未读消息
//! - [`FinishDiscussionTool`] — 主动结束讨论(等效于 `MessageKind::Finish`)
//!
//! 所有工具都通过 `MessageBus`(自身 `Clone`,内部 `Arc<Inner>`)共享状态;
//! `is_concurrency_safe()` 全部返回 `false`(`ToolExecutionQueue` 会强制串行
//! 调用,避免 mailbox 状态竞争)。
//!
//! `ToolError` 转译:`ToolError::InvalidArgs { message }` 用于参数缺失/类型错误;
//! `ToolError::Execution(String)` 用于 bus 路由失败。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use reflect_protocol::{ContentBlock, TokenUsage, ToolError, ToolOutput};
use reflect_tools::{Tool, ToolContext};

use crate::message_bus::MessageBus;
use crate::models::{AgentId, DiscussionMessage, MessageId, MessageKind};

/// `send_message(to?, content, kind?)` — 发送一条消息到指定 agent 或广播。
pub struct SendMessageTool {
    /// 当前 agent 的标识(注入消息 `from` 字段)。
    pub self_id: AgentId,
    /// 共享 bus(MessageBus 自身 `Clone`)。
    pub bus: MessageBus,
    /// 当前轮次计数器(由 [`DiscussionRuntime`](crate::runtime::DiscussionRuntime) 每轮 `store`,
    /// comm_tools 在 `execute()` 时 `load` 后写入 `DiscussionMessage.round`)。
    pub round: Arc<AtomicU32>,
    /// v0.2.4: 共享 token usage 槽。spawning thread 在
    /// `SpawnedChild::collect_result_with_usage().await` 之后写入本次 turn
    /// 的 usage;`execute()` 时把快照复制到出站 `DiscussionMessage.token_usage`。
    /// `None` 表示该 turn 还没有 LLM-reporter 注入 usage(早终止 / non-LLM 路径)。
    pub token_usage: Arc<Mutex<Option<TokenUsage>>>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to another agent in the discussion. Omit 'to' to broadcast to all other agents; pass a single agent role to unicast. 'kind' is one of utterance|consensus|finish (default utterance)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient agent role. Omit to broadcast to all."
                },
                "content": {
                    "type": "string",
                    "description": "Message body to send"
                },
                "kind": {
                    "type": "string",
                    "enum": ["utterance", "consensus", "finish"],
                    "default": "utterance",
                    "description": "Message kind (default utterance)"
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 共享 bus 状态,串行调用更安全。
        false
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'content'".into(),
            })?
            .to_string();
        let recipients: Vec<AgentId> = args
            .get("to")
            .and_then(|v| v.as_str())
            .map(|s| vec![AgentId(s.to_string())])
            .unwrap_or_default();
        let kind = match args
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("utterance")
        {
            "consensus" => MessageKind::Consensus,
            "finish" => MessageKind::Finish,
            _ => MessageKind::Utterance,
        };
        let discussion_id = self.bus.discussion_id();
        // v0.2.4: 把 spawning thread 写入的 TokenUsage 翻译成 DiscussionMessage
        // 兼容的 BTreeMap<String, u32>。空 map = non-LLM / early-abort 路径。
        let token_usage_map = self
            .token_usage
            .lock()
            .clone()
            .map(|u| {
                let mut m = std::collections::BTreeMap::new();
                m.insert("input".to_string(), u.input_tokens);
                m.insert("output".to_string(), u.output_tokens);
                m.insert("cached".to_string(), u.cached_tokens);
                m.insert("cache_write".to_string(), u.cache_write_tokens);
                m
            })
            .unwrap_or_default();
        let msg = DiscussionMessage {
            // id 由 bus.route 重写(0 是占位)
            id: MessageId(0),
            discussion_id,
            from: self.self_id.clone(),
            kind,
            content: content.clone(),
            recipients,
            // v0.2.3 起:round 由 runtime 在每轮开始前 store 到共享 AtomicU32,
            // comm_tools 在 execute 时读出当前值,v0 硬编码 0 已被替换。
            round: self.round.load(Ordering::SeqCst),
            // v0.2.4: 见上方 `token_usage_map` —— 来自 spawning thread 注入的
            // `TokenUsage` 槽,non-LLM 路径下为空 map。
            token_usage: token_usage_map,
        };
        self.bus
            .route(msg)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolOutput {
            content: vec![ContentBlock::Text {
                text: format!("sent ({} chars)", content.len()),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

/// `read_messages()` — 排空自己 mailbox 里的所有未读消息,返回纯文本。
pub struct ReadMessagesTool {
    pub self_id: AgentId,
    pub bus: MessageBus,
}

#[async_trait]
impl Tool for ReadMessagesTool {
    fn name(&self) -> &str {
        "read_messages"
    }

    fn description(&self) -> &str {
        "Drain all unread messages from your mailbox. Returns one line per message in the form '[<kind>] <from>: <content>'."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        // 先检查 mailbox 存在,再排空(closure 形式)
        if self.bus.mailbox(&self.self_id).is_none() {
            return Err(ToolError::Execution(format!(
                "no mailbox for {}",
                self.self_id.0
            )));
        }
        let mut out = String::new();
        self.bus
            .with_mailbox_mut(&self.self_id, |mb| {
                while let Ok(m) = mb.try_recv() {
                    out.push_str(&format!("[{:?}] {}: {}\n", m.kind, m.from.0, m.content));
                }
            })
            .expect("mailbox existence was just checked");
        Ok(ToolOutput {
            content: vec![ContentBlock::Text { text: out }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

/// `finish_discussion(summary)` — 主动结束讨论,记录 `summary` 到 transcript。
///
/// 第一次调用成功时把 `finished` 标志置 true;重复调用返回 `"already finished"`
/// 提示(不二次追加 Finish 消息,避免 transcript 重复)。
pub struct FinishDiscussionTool {
    pub self_id: AgentId,
    pub bus: MessageBus,
    /// 一次性信号:第一次成功调用置 true,第二次返回 "already finished"。
    pub finished: Arc<Mutex<bool>>,
    /// 当前轮次计数器(同 [`SendMessageTool::round`])。
    pub round: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for FinishDiscussionTool {
    fn name(&self) -> &str {
        "finish_discussion"
    }

    fn description(&self) -> &str {
        "End the discussion early with a final summary. Idempotent: a second call returns 'already finished' without appending a duplicate Finish message."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Final summary of the discussion"
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        if *self.finished.lock() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text {
                    text: "already finished".into(),
                }],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 0,
            });
        }
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'summary'".into(),
            })?
            .to_string();
        let discussion_id = self.bus.discussion_id();
        let msg = DiscussionMessage {
            id: MessageId(0),
            discussion_id,
            from: self.self_id.clone(),
            kind: MessageKind::Finish,
            content: summary.clone(),
            recipients: vec![],
            // v0.2.3 起:从共享 AtomicU32 读取当前 round(同 SendMessageTool)
            round: self.round.load(Ordering::SeqCst),
            token_usage: Default::default(),
        };
        self.bus
            .route(msg)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        *self.finished.lock() = true;
        Ok(ToolOutput {
            content: vec![ContentBlock::Text {
                text: format!("finished: {}", summary),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use crate::models::DiscussionId;
    use reflect_tools::ToolContext;

    #[allow(clippy::type_complexity)]
    fn mk_bus_and_tools(
        self_id: AgentId,
        other: AgentId,
    ) -> (
        MessageBus,
        Arc<Mutex<bool>>,
        Arc<AtomicU32>,
        Arc<Mutex<Option<TokenUsage>>>,
        SendMessageTool,
        ReadMessagesTool,
        FinishDiscussionTool,
    ) {
        let bus = MessageBus::new(DiscussionId::new(), vec![self_id.clone(), other], 8);
        let finished = Arc::new(Mutex::new(false));
        let round = Arc::new(AtomicU32::new(0));
        let token_usage_slot = Arc::new(Mutex::new(None));
        let send = SendMessageTool {
            self_id: self_id.clone(),
            bus: bus.clone(),
            round: round.clone(),
            token_usage: token_usage_slot.clone(),
        };
        let read = ReadMessagesTool {
            self_id: self_id.clone(),
            bus: bus.clone(),
        };
        let finish = FinishDiscussionTool {
            self_id,
            bus: bus.clone(),
            finished: finished.clone(),
            round: round.clone(),
        };
        (bus, finished, round, token_usage_slot, send, read, finish)
    }

    fn default_ctx() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn send_message_broadcast_routes_to_other_agent() {
        let (bus, _finished, _round, _token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        send.execute(
            default_ctx(),
            serde_json::json!({"content": "hi all", "kind": "utterance"}),
        )
        .await
        .unwrap();
        // b 的 mailbox 应该收到
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let b_msg = b_msg.expect("b should have received the broadcast");
        assert_eq!(b_msg.content, "hi all");
        // a 的 mailbox 应该空(broadcast 排除 sender)
        let a_msg = bus
            .with_mailbox_mut(&AgentId("a".into()), |mb| mb.try_recv().ok())
            .expect("a mailbox exists");
        assert!(
            a_msg.is_none(),
            "sender should not receive their own broadcast"
        );
    }

    #[tokio::test]
    async fn send_message_unicast_routes_only_to_named() {
        // 3-agent 总线
        let bus = MessageBus::new(
            DiscussionId::new(),
            vec![
                AgentId("a".into()),
                AgentId("b".into()),
                AgentId("c".into()),
            ],
            8,
        );
        let send = SendMessageTool {
            self_id: AgentId("a".into()),
            bus: bus.clone(),
            round: Arc::new(AtomicU32::new(0)),
            token_usage: Arc::new(Mutex::new(None)),
        };
        send.execute(
            default_ctx(),
            serde_json::json!({"content": "private to b", "to": "b"}),
        )
        .await
        .unwrap();
        // 只有 b 收到
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let b_msg = b_msg.expect("b should have received the unicast");
        assert_eq!(b_msg.content, "private to b");
        let a_msg = bus
            .with_mailbox_mut(&AgentId("a".into()), |mb| mb.try_recv().ok())
            .expect("a mailbox exists");
        assert!(a_msg.is_none(), "a should not receive unicast not for them");
        let c_msg = bus
            .with_mailbox_mut(&AgentId("c".into()), |mb| mb.try_recv().ok())
            .expect("c mailbox exists");
        assert!(c_msg.is_none(), "c should not receive unicast not for them");
    }

    #[tokio::test]
    async fn send_message_missing_content_returns_invalid_args() {
        let (_bus, _finished, _round, _token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        let err = send
            .execute(default_ctx(), serde_json::json!({"kind": "utterance"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { .. }),
            "expected InvalidArgs, got {err:?}",
        );
    }

    #[tokio::test]
    async fn read_messages_drains_mailbox_in_fifo_order() {
        let (bus, _finished, _round, _token_slot, _send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        // 让 a 收到 b 的两条消息
        {
            let b = bus.clone();
            b.route(DiscussionMessage {
                id: Default::default(),
                discussion_id: b.discussion_id(),
                from: AgentId("b".into()),
                kind: MessageKind::Utterance,
                content: "first".into(),
                recipients: vec![AgentId("a".into())],
                round: 0,
                token_usage: Default::default(),
            })
            .await
            .unwrap();
            b.route(DiscussionMessage {
                id: Default::default(),
                discussion_id: b.discussion_id(),
                from: AgentId("b".into()),
                kind: MessageKind::Utterance,
                content: "second".into(),
                recipients: vec![AgentId("a".into())],
                round: 0,
                token_usage: Default::default(),
            })
            .await
            .unwrap();
        }
        // a 调 read_messages,应该排空
        let read = ReadMessagesTool {
            self_id: AgentId("a".into()),
            bus: bus.clone(),
        };
        let out = read
            .execute(default_ctx(), serde_json::json!({}))
            .await
            .unwrap();
        let text = match &out.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("first"), "got: {text}");
        assert!(text.contains("second"), "got: {text}");
        // mailbox 应该空了
        let a_msg = bus
            .with_mailbox_mut(&AgentId("a".into()), |mb| mb.try_recv().ok())
            .expect("a mailbox exists");
        assert!(
            a_msg.is_none(),
            "mailbox should be empty after read_messages"
        );
    }

    #[tokio::test]
    async fn read_messages_no_mailbox_returns_execution_error() {
        // self_id 在 bus 里没注册
        let bus = MessageBus::new(DiscussionId::new(), vec![AgentId("a".into())], 4);
        let read = ReadMessagesTool {
            self_id: AgentId("ghost".into()),
            bus: bus.clone(),
        };
        let err = read
            .execute(default_ctx(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn finish_discussion_records_summary_and_sets_flag() {
        let (bus, finished, _round, _token_slot, _send, _read, finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        let out = finish
            .execute(default_ctx(), serde_json::json!({"summary": "we agreed"}))
            .await
            .unwrap();
        let text = match &out.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("finished: we agreed"), "got: {text}");
        // finished flag 置 true
        assert!(*finished.lock(), "finished flag should be set");
        // transcript 多了一条 Finish 消息
        let t = bus.transcript();
        assert_eq!(t.len(), 1);
        assert!(matches!(t[0].kind, MessageKind::Finish));
        assert_eq!(t[0].content, "we agreed");
    }

    #[tokio::test]
    async fn finish_discussion_second_call_returns_already_finished() {
        let (_bus, finished, _round, _token_slot, _send, _read, finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        // 第一次成功
        finish
            .execute(default_ctx(), serde_json::json!({"summary": "x"}))
            .await
            .unwrap();
        // 第二次返回 already finished,不追加 transcript
        let out = finish
            .execute(default_ctx(), serde_json::json!({"summary": "y"}))
            .await
            .unwrap();
        let text = match &out.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(text, "already finished");
        // finished 仍然 true
        assert!(*finished.lock());
    }

    #[tokio::test]
    async fn finish_discussion_missing_summary_returns_invalid_args() {
        let (_bus, _finished, _round, _token_slot, _send, _read, finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        let err = finish
            .execute(default_ctx(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got {err:?}");
    }

    // ── v0.2.3 新增:round 标记测试 ──────────────────────────────────────

    /// v0.2.3 默认 round = 0(AtomicU32 新建未 store 时为 0)。
    #[tokio::test]
    async fn send_message_round_atomic_zero_when_unset() {
        let (bus, _finished, _round, _token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        send.execute(
            default_ctx(),
            serde_json::json!({"content": "hi", "kind": "utterance"}),
        )
        .await
        .unwrap();
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let m = b_msg.expect("b should have received the message");
        assert_eq!(m.round, 0, "fresh AtomicU32 should yield round=0");
    }

    /// v0.2.3 起:SendMessageTool 在 execute 时读取共享 AtomicU32,runtime
    /// 每轮 `store(round)`,消息 `round` 字段反映 runtime 当前轮次。
    #[tokio::test]
    async fn send_message_round_atomic_reflects_runtime_state() {
        let (bus, _finished, round, _token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        // 模拟 runtime 跑到第 2 轮
        round.store(2, Ordering::SeqCst);
        send.execute(default_ctx(), serde_json::json!({"content": "round 2 msg"}))
            .await
            .unwrap();
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let m = b_msg.expect("b should have received");
        assert_eq!(m.round, 2, "SendMessageTool must read current round");
    }

    /// v0.2.3 起:FinishDiscussionTool 同样读共享 AtomicU32 写入 round 字段。
    #[tokio::test]
    async fn finish_discussion_round_atomic_reflects_runtime_state() {
        let (bus, _finished, round, _token_slot, _send, _read, finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        round.store(3, Ordering::SeqCst);
        finish
            .execute(default_ctx(), serde_json::json!({"summary": "all agreed"}))
            .await
            .unwrap();
        let t = bus.transcript();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].round, 3,
            "FinishDiscussionTool must read current round"
        );
        assert!(matches!(t[0].kind, MessageKind::Finish));
    }

    // ── v0.2.4 新增:token_usage 注入测试 ─────────────────────────────

    /// v0.2.4:`SendMessageTool` 在 execute 时读取共享 `token_usage` 槽,并
    /// 把 `TokenUsage` 字段翻译成 `DiscussionMessage.token_usage` 的 BTreeMap。
    /// 槽为 None 时 map 为空。
    #[tokio::test]
    async fn send_message_injects_token_usage_into_message() {
        let (bus, _finished, _round, token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        // 模拟 spawning thread 写入了本轮 usage
        *token_slot.lock() = Some(TokenUsage::new(123, 45, 10));
        send.execute(
            default_ctx(),
            serde_json::json!({"content": "round 1 msg", "kind": "utterance"}),
        )
        .await
        .unwrap();
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let m = b_msg.expect("b should have received the message");
        // 应当有 input / output / cached / cache_write 四个 key
        assert_eq!(m.token_usage.get("input").copied(), Some(123));
        assert_eq!(m.token_usage.get("output").copied(), Some(45));
        assert_eq!(m.token_usage.get("cached").copied(), Some(10));
    }

    /// v0.2.4:槽为 None 时(早终止 / non-LLM 路径),消息 token_usage 应为空 map。
    #[tokio::test]
    async fn send_message_default_token_usage_is_empty_map() {
        let (bus, _finished, _round, token_slot, send, _read, _finish) =
            mk_bus_and_tools(AgentId("a".into()), AgentId("b".into()));
        // token_slot 默认就是 None,这里显式重置一次
        *token_slot.lock() = None;
        send.execute(default_ctx(), serde_json::json!({"content": "non-llm msg"}))
            .await
            .unwrap();
        let b_msg = bus
            .with_mailbox_mut(&AgentId("b".into()), |mb| mb.try_recv().ok())
            .expect("b mailbox exists");
        let m = b_msg.expect("b should have received");
        assert!(
            m.token_usage.is_empty(),
            "empty slot must produce empty map, got: {:?}",
            m.token_usage
        );
    }
}
