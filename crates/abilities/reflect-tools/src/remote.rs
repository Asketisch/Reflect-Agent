//! `RemoteTool` —— 客户端(serve 模式)注册的跨语言自定义工具。
//!
//! 架构:工具的**实现留在客户端进程**(Python / TS 函数),core 侧只持
//! 有 [`RemoteTool`] 适配器。LLM 调用时,适配器经 [`RemoteBridge`] emit
//! `EventMsg::ToolExecutionRequest`;客户端本地执行 handler 后回
//! `Op::ToolExecutionResponse`,serve 把回执投递回桥,完成 oneshot。
//!
//! 等待语义与 `ApprovalGate::ask_tool` 同构:cancel token / 超时 /
//! 事件通道关闭三路短路,超时返回 `ToolError::Execution`。
//!
//! 桥由 serve 进程持有(每个 serve 一个);`events()` 取出唯一的接收端,
//! serve 把它合并进 stdout JSONL writer。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use reflect_protocol::{
    ContentBlock, EVENT_ID_NONE, Event, EventMsg, PermissionMode, RemoteToolSpec, ToolError,
    ToolExecutionRequestEvent, ToolOutput,
};

use crate::tool::{Tool, ToolContext};

/// 待处理远程工具执行的 `call_id` → oneshot waiter 映射。
type RemoteWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<ToolOutput>>>>;

/// 远程工具桥:一条独立事件通道 + waiter 表。
///
/// `event_tx` 供 `RemoteTool::execute` 发请求事件;`events()` 只应被
/// serve 调一次,取走接收端并转发到 stdout。
pub struct RemoteBridge {
    event_tx: mpsc::Sender<Event>,
    waiters: RemoteWaiters,
}

impl RemoteBridge {
    /// 构造桥,返回 `(Arc<桥>, 事件接收端)`。
    pub fn new(channel_capacity: usize) -> (Arc<Self>, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(channel_capacity);
        (
            Arc::new(Self {
                event_tx: tx,
                waiters: Arc::new(Mutex::new(HashMap::new())),
            }),
            rx,
        )
    }

    /// 发起一次远程工具执行:emit `ToolExecutionRequest` 并等待客户端
    /// 回执 / cancel / 超时。`timeout` 为零表示不限时(只受 cancel 与
    /// 通道关闭兜底)。
    pub async fn request(
        &self,
        tool: &str,
        args: Value,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<ToolOutput, ToolError> {
        let call_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ToolOutput>();
        self.waiters.lock().insert(call_id.clone(), tx);

        let ev = Event::new(
            EVENT_ID_NONE,
            EventMsg::ToolExecutionRequest(ToolExecutionRequestEvent {
                call_id: call_id.clone(),
                tool: tool.to_string(),
                args: args.clone(),
            }),
        );
        if self.event_tx.send(ev).await.is_err() {
            // 事件通道关闭 —— serve 已退出,没有任何人会回执。
            self.waiters.lock().remove(&call_id);
            return Err(ToolError::Execution(
                "remote tool: event channel closed (serve exiting?)".into(),
            ));
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.waiters.lock().remove(&call_id);
                Err(ToolError::Cancelled)
            }
            _ = tokio::time::sleep(timeout), if !timeout.is_zero() => {
                self.waiters.lock().remove(&call_id);
                Err(ToolError::Execution(format!(
                    "remote tool '{tool}': timed out after {}ms",
                    timeout.as_millis()
                )))
            }
            res = rx => {
                res.map_err(|_| ToolError::Execution(format!(
                    "remote tool '{tool}': waiter dropped before response"
                )))
            }
        }
    }

    /// 投递客户端回执(serve 收到 `Op::ToolExecutionResponse` 时调用)。
    /// 返回 `false` 表示没有匹配的 waiter(已超时 / 取消 / 未知 call_id)。
    pub fn complete(&self, call_id: &str, output: ToolOutput) -> bool {
        match self.waiters.lock().remove(call_id) {
            Some(tx) => tx.send(output).is_ok(),
            None => {
                tracing::debug!(
                    call_id = %call_id,
                    "remote tool response arrived but no waiter (cancelled?)"
                );
                false
            }
        }
    }
}

/// 客户端自定义工具的 core 侧适配器。由 serve 在收到
/// `Op::RegisterTools` 时构造并 `register_runtime_tool` 到 `ToolRegistry`。
pub struct RemoteTool {
    spec: RemoteToolSpec,
    bridge: Arc<RemoteBridge>,
    timeout: Duration,
}

impl RemoteTool {
    /// 从客户端提供的 spec 构造。`timeout` 是单次执行等待客户端回执的
    /// 上限(serve 从 env 读取,默认 120s)。
    pub fn new(spec: RemoteToolSpec, bridge: Arc<RemoteBridge>, timeout: Duration) -> Self {
        Self {
            spec,
            bridge,
            timeout,
        }
    }

    /// 工具名(与注册 spec 一致)。
    pub fn name(&self) -> &str {
        &self.spec.name
    }
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn parameters_schema(&self) -> Value {
        self.spec.parameters.clone()
    }

    /// 客户端本地执行,无 core 侧共享副作用,可并发。
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    /// 客户端自定义工具默认 `Auto`:是否放行由客户端自己的 handler 决定,
    /// core 侧审批没有意义(审批的对象应是本进程内有副作用的操作)。
    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        match self
            .bridge
            .request(&self.spec.name, args, &ctx.cancel, self.timeout)
            .await
        {
            Ok(output) => Ok(output),
            Err(e) => {
                // 失败也以 ToolOutput 形式回给 LLM(而非硬错误),让模型
                // 能看到失败原因并自行调整;与内置工具的错误返回风格一致。
                let message = match &e {
                    ToolError::Execution(msg) => msg.clone(),
                    ToolError::InvalidArgs { message } => message.clone(),
                    other => format!("{other:?}"),
                };
                Ok(ToolOutput {
                    content: vec![ContentBlock::text(format!(
                        "remote tool '{}' failed: {message}",
                        self.spec.name
                    ))],
                    is_error: true,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从事件流里取出一条 ToolExecutionRequest(测试辅助)。
    async fn next_request(rx: &mut mpsc::Receiver<Event>) -> ToolExecutionRequestEvent {
        while let Some(ev) = rx.recv().await {
            if let EventMsg::ToolExecutionRequest(req) = ev.msg {
                return req;
            }
        }
        panic!("事件流关闭前未出现 ToolExecutionRequest");
    }

    #[tokio::test]
    async fn 桥上请求与回执_往返成功() {
        let (bridge, mut rx) = RemoteBridge::new(8);
        let bridge2 = bridge.clone();
        let cancel = CancellationToken::new();

        let task = tokio::spawn(async move {
            bridge2
                .request(
                    "get_weather",
                    serde_json::json!({"city": "北京"}),
                    &cancel,
                    Duration::from_secs(5),
                )
                .await
        });

        let req = next_request(&mut rx).await;
        assert_eq!(req.tool, "get_weather");
        assert_eq!(req.args["city"], "北京");

        let ok = bridge.complete(
            &req.call_id,
            ToolOutput {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
                metadata: serde_json::json!({}),
                elapsed_ms: 0,
            },
        );
        assert!(ok);

        let output = task.await.unwrap().expect("应成功拿到回执");
        assert!(!output.is_error);
        assert_eq!(output.content.len(), 1);
    }

    fn 空_output() -> ToolOutput {
        ToolOutput {
            content: vec![],
            is_error: false,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        }
    }

    #[tokio::test]
    async fn 未知_call_id_回执返回_false() {
        let (bridge, _rx) = RemoteBridge::new(8);
        assert!(!bridge.complete("no-such-id", 空_output()));
    }

    #[tokio::test]
    async fn 超时返回执行错误() {
        let (bridge, mut rx) = RemoteBridge::new(8);
        let cancel = CancellationToken::new();
        let bridge2 = bridge.clone();

        let task = tokio::spawn(async move {
            bridge2
                .request(
                    "slow_tool",
                    serde_json::json!({}),
                    &cancel,
                    Duration::from_millis(50),
                )
                .await
        });

        // 收到请求但不回执 —— 等待超时。
        let req = next_request(&mut rx).await;
        let err = task.await.unwrap().expect_err("应超时失败");
        match err {
            ToolError::Execution(msg) => assert!(msg.contains("timed out")),
            other => panic!("wrong error: {other:?}"),
        }
        // 超时后 waiter 已清理:迟到的回执返回 false。
        assert!(!bridge.complete(&req.call_id, 空_output()));
    }

    #[tokio::test]
    async fn cancel_令牌立即短路() {
        let (bridge, _rx) = RemoteBridge::new(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = bridge
            .request("t", serde_json::json!({}), &cancel, Duration::from_secs(60))
            .await
            .expect_err("cancel 后应失败");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn remote_tool_失败包装为_is_error_输出() {
        let (bridge, mut rx) = RemoteBridge::new(8);
        let tool = RemoteTool::new(
            RemoteToolSpec {
                name: "boom".into(),
                description: "总会失败".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            bridge.clone(),
            Duration::from_millis(50),
        );
        assert_eq!(tool.name(), "boom");
        assert!(tool.is_concurrency_safe());

        let task = tokio::spawn(async move {
            tool.execute(ToolContext::default(), serde_json::json!({"x": 1}))
                .await
        });

        // 收到请求但不回执,制造超时 —— execute 应返回 is_error 的
        // ToolOutput(而非 Err),让 LLM 看到失败原因。
        let _req = next_request(&mut rx).await;
        let output = task.await.unwrap().expect("execute 应返回 Ok");
        assert!(output.is_error);
    }

    /// 事件流断言辅助:确认请求事件落在 Event 的 wire 形态上
    /// (`type == "tool_execution_request"`)。
    #[tokio::test]
    async fn 请求事件_序列化带_type_标签() {
        let (bridge, mut rx) = RemoteBridge::new(8);
        let cancel = CancellationToken::new();
        let _ = bridge
            .request(
                "t",
                serde_json::json!({}),
                &cancel,
                Duration::from_millis(10),
            )
            .await;
        let ev = rx.recv().await.expect("应有事件");
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(r#""type":"tool_execution_request""#),
            "got: {json}"
        );
    }
}
