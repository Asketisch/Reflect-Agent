//! `ToolExecutionQueue` 的集成测试(含 hook 集成)。

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use reflect_hooks::{Hook, HookDecision, HookEngine, HookError, HookEvent, HookEventKind};
use reflect_protocol::{ContentBlock, ToolError, ToolOutput};
use reflect_tools::{Tool, ToolCallRequest, ToolContext, ToolExecutionQueue, ToolRegistry};

struct StubTool {
    name: String,
    safe: bool,
}
#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "stub"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        self.safe
    }
    async fn execute(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![ContentBlock::text(format!("from {}", self.name))],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

struct DenyAllHook;
#[async_trait]
impl Hook for DenyAllHook {
    fn name(&self) -> &str {
        "deny_all"
    }
    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse]
    }
    async fn handle(&self, _: &HookEvent) -> Result<HookDecision, HookError> {
        Ok(HookDecision::Deny {
            reason: "blocked by deny_all".into(),
        })
    }
}

struct CountHook {
    count: Arc<Mutex<u32>>,
}
#[async_trait]
impl Hook for CountHook {
    fn name(&self) -> &str {
        "counter"
    }
    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse, HookEventKind::PostToolUse]
    }
    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        *self.count.lock() += 1;
        let _ = event; // suppress unused
        Ok(HookDecision::Allow)
    }
}

fn ctx() -> ToolContext {
    ToolContext::for_workspace(".")
}

#[tokio::test]
async fn pre_tool_use_deny_blocks_execution() {
    let reg = Arc::new(ToolRegistry::default());
    reg.register(Arc::new(StubTool {
        name: "echo".into(),
        safe: true,
    }));
    let engine = Arc::new(HookEngine::new());
    engine.register(DenyAllHook);
    let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());

    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert_eq!(out.len(), 1);
    assert!(out[0].is_error);
    assert!(
        out[0]
            .content
            .iter()
            .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("Denied by hook")))
    );
}

#[tokio::test]
async fn pre_and_post_hooks_fire() {
    let reg = Arc::new(ToolRegistry::default());
    reg.register(Arc::new(StubTool {
        name: "echo".into(),
        safe: true,
    }));
    let engine = Arc::new(HookEngine::new());
    let count = Arc::new(Mutex::new(0u32));
    engine.register(CountHook {
        count: count.clone(),
    });
    let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());

    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert!(!out[0].is_error);
    // PreToolUse + PostToolUse = 2 calls
    assert_eq!(*count.lock(), 2);
}

#[tokio::test]
async fn error_triggers_post_tool_use_failure() {
    use reflect_tools::ToolError;
    struct FailingTool;
    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "fails"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Execution("intentional".into()))
        }
    }
    let reg = Arc::new(ToolRegistry::default());
    reg.register(Arc::new(FailingTool));
    let engine = Arc::new(HookEngine::new());
    struct FailCount {
        n: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl Hook for FailCount {
        fn name(&self) -> &str {
            "fail_count"
        }
        fn events(&self) -> &[HookEventKind] {
            &[HookEventKind::PostToolUseFailure]
        }
        async fn handle(&self, _: &HookEvent) -> Result<HookDecision, HookError> {
            *self.n.lock() += 1;
            Ok(HookDecision::Allow)
        }
    }
    let n = Arc::new(Mutex::new(0u32));
    engine.register(FailCount { n: n.clone() });
    let q = ToolExecutionQueue::with_defaults(reg, engine, ctx());

    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "1".into(),
            name: "fail".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert!(out[0].is_error);
    assert_eq!(*n.lock(), 1);
}
