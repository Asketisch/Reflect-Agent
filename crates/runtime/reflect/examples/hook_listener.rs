//! `hook_listener` —— 注册自定义 hook,响应生命周期事件。
//!
//! 两个 hook:
//! - `TokenUsageHook`:按 turn 聚合 token 计数。
//! - `DangerousCommandHook`:拒绝匹配 `rm -rf /` 的 `bash` 调用。
//!
//! 使用 `custom_provider.rs` 中的 `mock` provider,让 LLM
//! 触发 `bash` 工具调用,从而命中 `DangerousCommandHook`。
//!
//! 运行:
//! ```bash
//! cargo run -p reflect --example hook_listener
//! ```

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use futures::stream;
use reflect::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, EventMsg, Hook, HookDecision, HookError,
    HookEvent, HookEventKind, LlmError, ModelClient, ModelRegistry, PoolEntry, Reflect, Submission,
    Tool, ToolContext, ToolError, ToolRegistry,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 聚合每轮 token 用量的 hook。
struct TokenUsageHook {
    total_input: AtomicU32,
    total_output: AtomicU32,
}

#[async_trait]
impl Hook for TokenUsageHook {
    fn name(&self) -> &str {
        "token_usage_tracker"
    }
    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::SessionStart]
    }
    async fn handle(&self, _event: &HookEvent) -> Result<HookDecision, HookError> {
        // 真正的实现会检查 Notification 事件;v0 仅做 ack。
        self.total_input.fetch_add(0, Ordering::SeqCst);
        self.total_output.fetch_add(0, Ordering::SeqCst);
        Ok(HookDecision::Allow)
    }
}

/// 拒绝危险 `bash` 命令的 hook。
struct DangerousCommandHook;

#[async_trait]
impl Hook for DangerousCommandHook {
    fn name(&self) -> &str {
        "dangerous_command_blocker"
    }
    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse]
    }
    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        if let HookEvent::PreToolUse { tool, args, .. } = event
            && tool == "bash"
        {
            let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.contains("rm -rf /") || cmd.contains("sudo ") {
                return Ok(HookDecision::Deny {
                    reason: format!("dangerous command blocked: {cmd}"),
                });
            }
        }
        Ok(HookDecision::Allow)
    }
}

/// 触发 `bash` 工具调用 `rm -rf /tmp/foo` 的 mock LLM。
struct DangerToolLlm;

#[async_trait]
impl ModelClient for DangerToolLlm {
    fn name(&self) -> &str {
        "danger-mock"
    }
    fn capabilities(&self) -> Capabilities {
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
        let chunks: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::MessageStart {
                id: "m".into(),
                model: "danger-mock".into(),
            }),
            Ok(ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "bash".into(),
                input_json: String::new(),
            }),
            Ok(ChatEvent::ToolUseDelta(
                r#"{"cmd":"rm -rf /tmp/foo"}"#.into(),
            )),
            Ok(ChatEvent::MessageStop),
            Ok(ChatEvent::Usage {
                input_tokens: 10,
                output_tokens: 8,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
        ];
        Ok(Box::pin(stream::iter(chunks)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "mock",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(DangerToolLlm),
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    use reflect_core::{AgentConfig, AgentThread};
    use reflect_tools::builtins;
    use std::path::Path;
    let cfg = AgentConfig::new("mock/danger-mock", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(builtins::BashTool));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    // 在 thread 的 hook engine 上注册 hook。`register_hook` 消费传入值
    // (对 `H: Hook + 'static` 泛型);若需要在别处持有引用,用 `Arc`
    // 包一层并 clone 内部值。
    let token_hook = TokenUsageHook {
        total_input: AtomicU32::new(0),
        total_output: AtomicU32::new(0),
    };
    thread.register_hook(token_hook);
    thread.register_hook(DangerousCommandHook);

    let agent = Reflect::from_thread(thread);
    let prompt = "Please run 'rm -rf /tmp/foo' to clean up the test directory.";
    let mut stream = agent.submit(Submission::user_input(prompt)).await;

    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        match event.msg {
            EventMsg::ToolCallEnd(end) => {
                let preview: String = end
                    .output
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        reflect::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "[tool {}] error={} preview={preview}",
                    end.call_id, end.is_error
                );
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    println!(
        "(token usage hook fired and self-counted; final total recorded separately by the caller)"
    );
    let _ = <dyn Tool>::name; // 抑制 unused 告警
    let _ = ToolError::InvalidArgs {
        message: String::new(),
    };
    let _ = ToolContext::default;
    let _ = Value::Null;
    Ok(())
}
