//! v1.4 C1 — `Op::QuerySubagents` 状态查询端到端。
//!
//! 客户端提交查询 → submission_loop 从子代理状态中心取快照 →
//! `EventMsg::SubagentStatus` 经 per-turn 通道送达。此处预登记一个
//! 状态槽(模拟在飞子代理),断言快照字段忠实回显。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, stream};
use reflect_core::{AgentConfig, AgentThread, SubagentRuntimeRegistry};
use reflect_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use reflect_protocol::{EventMsg, Op, SubagentRunStateMirror, Submission};
use reflect_tools::ToolRegistry;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct StubClient;

#[async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        Ok(Box::pin(stream::iter(vec![
            Ok(ChatEvent::MessageStart {
                id: "m".into(),
                model: "stub-1".into(),
            }),
            Ok(ChatEvent::MessageStop),
        ])))
    }
}

fn sub(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
    }
}

#[tokio::test]
async fn query_subagents_returns_status_snapshot() {
    let runtime = Arc::new(SubagentRuntimeRegistry::new());
    // 预登记一个「在飞」子代理槽(模拟父会话 factory spawn 过)。
    let slot = runtime.register("child-abc", "explorer", CancellationToken::new());
    slot.begin_tool("grep");
    slot.set_iteration(2);
    slot.add_tokens(555);

    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_subagent_runtime(runtime.clone());
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    // 查询全部。
    let mut handle = thread
        .submit(sub("q1", Op::QuerySubagents { child_id: None }))
        .await;
    let deadline = std::time::Duration::from_secs(5);
    let mut found = false;
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event in time")
    {
        if let EventMsg::SubagentStatus(st) = ev.msg {
            assert_eq!(st.children.len(), 1, "应返回登记的单个子代理");
            let c = &st.children[0];
            assert_eq!(c.child_id, "child-abc");
            assert_eq!(c.role, "explorer");
            assert_eq!(c.state, SubagentRunStateMirror::Running);
            assert_eq!(c.current_tool.as_deref(), Some("grep"));
            assert_eq!(c.iteration, 2);
            assert_eq!(c.total_tokens, 555);
            found = true;
            break;
        }
    }
    assert!(found, "应收到 SubagentStatus 事件");

    // 定向查询未知 id → 空列表。
    let mut handle = thread
        .submit(sub(
            "q2",
            Op::QuerySubagents {
                child_id: Some("ghost".into()),
            },
        ))
        .await;
    while let Some(ev) = timeout(deadline, handle.next())
        .await
        .expect("event in time")
    {
        if let EventMsg::SubagentStatus(st) = ev.msg {
            assert!(st.children.is_empty(), "未知 child_id 应返回空列表");
            break;
        }
    }
}
