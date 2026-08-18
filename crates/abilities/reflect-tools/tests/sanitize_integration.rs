//! Secret Sanitization 的 queue 集成测试 —— 端到端验证脱敏发生在
//! `ToolExecutionQueue::execute_single` 的 `Ok(Ok(_))` 分支,覆盖:
//!
//! - stub tool 返回的假密钥在最终 `ToolResult.content` 中被替换为 marker
//! - 错误 / 超时分支合成的 `ContentBlock::text(...)` 不被二次扫描
//! - `Sanitizer::disabled()` 完全 no-op,原文字节保留
//! - nested `ContentBlock::ToolResult` 递归脱敏(sub-agent 重发兜底)
//! - 自定义 `PostToolUse` hook 收到的是脱敏后版本,看不到原始密钥
//!
//! 这些是 Sub-task 3 的核心验收条件。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_hooks::{Hook, HookDecision, HookEngine, HookEvent};
use reflect_protocol::{ContentBlock, ToolError, ToolOutput};
use reflect_tools::{
    Tool, ToolContext, ToolExecutionQueue, ToolRegistry, queue::ToolCallRequest,
    sanitize::Sanitizer,
};

// ── Stub(桩实现)──────────────────────────────

/// 假装 leaky 的工具,`mode` 决定返回哪类假密钥。
struct LeakyTool {
    name: String,
    payload: &'static str,
}

#[async_trait]
impl Tool for LeakyTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "leaky tool emitting fake credentials"
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
        Ok(ToolOutput {
            content: vec![ContentBlock::text(self.payload)],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

/// 假装失败的工具,`is_error: true` 时仍带 text payload(测试错误分支不被二次扫描)。
struct ErrorLeakyTool {
    name: String,
    payload: &'static str,
}

#[async_trait]
impl Tool for ErrorLeakyTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "leaky tool that errors out"
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
        Ok(ToolOutput {
            content: vec![ContentBlock::text(self.payload)],
            is_error: true,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

/// 嵌套结构工具 —— 输出 `ContentBlock::ToolResult` 形式,内层仍含密钥。
struct NestedLeakyTool;

#[async_trait]
impl Tool for NestedLeakyTool {
    fn name(&self) -> &str {
        "nested_leaky"
    }
    fn description(&self) -> &str {
        "leaky tool emitting nested ToolResult block"
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
        // 顶层是 ToolResult 块,内层 Text 含假 key —— 测试递归脱敏。
        Ok(ToolOutput {
            content: vec![ContentBlock::ToolResult {
                call_id: "inner".into(),
                output: ToolOutput {
                    content: vec![ContentBlock::text("API_KEY=secret")],
                    is_error: false,
                    metadata: serde_json::json!({}),
                    elapsed_ms: 0,
                },
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

// ── Hook 用于检查 PostToolUse 看到的版本 ─────────────────────────

use std::sync::Mutex;

/// `PostToolUse` hook,把看到的 `result.content` 文本缓存到共享变量。
/// 测试主线程后续读取,断言看到的是脱敏后版本。
struct CapturingHook {
    captured: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Hook for CapturingHook {
    fn name(&self) -> &str {
        "capturing_post_tool_use"
    }
    fn events(&self) -> &[reflect_hooks::HookEventKind] {
        &[reflect_hooks::HookEventKind::PostToolUse]
    }
    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, reflect_hooks::HookError> {
        if let HookEvent::PostToolUse { result, .. } = event {
            for block in &result.content {
                if let ContentBlock::Text { text } = block {
                    self.captured.lock().unwrap().push(text.clone());
                }
            }
        }
        Ok(HookDecision::Allow)
    }
}

// ── 辅助函数 ─────────────────────────────────────────────────────

fn make_ctx() -> ToolContext {
    ToolContext::for_workspace(".")
}

fn build_queue_with(tool: Arc<dyn Tool>, sanitizer: Arc<Sanitizer>) -> ToolExecutionQueue {
    let reg = Arc::new(ToolRegistry::default());
    reg.register(tool);
    let engine = Arc::new(HookEngine::new());
    ToolExecutionQueue::with_sanitizer(reg, engine, make_ctx(), sanitizer)
}

fn build_queue_with_hook(
    tool: Arc<dyn Tool>,
    sanitizer: Arc<Sanitizer>,
    captured: Arc<Mutex<Vec<String>>>,
) -> (ToolExecutionQueue, Arc<Mutex<Vec<String>>>) {
    let reg = Arc::new(ToolRegistry::default());
    reg.register(tool);
    let engine = Arc::new(HookEngine::new());
    // 注册 CapturingHook 到 HookEngine(owned H,而非 Arc<dyn Hook>)。
    engine.register(CapturingHook {
        captured: captured.clone(),
    });
    let q = ToolExecutionQueue::with_sanitizer(reg, engine, make_ctx(), sanitizer);
    (q, captured)
}

// ── 测试 ──────────────────────────────────────────────────────

#[tokio::test]
async fn queue_redacts_aws_key_end_to_end() {
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "aws_leak".into(),
        payload: "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "aws_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert_eq!(out.len(), 1);
    assert!(!out[0].is_error);
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            // AWS pattern 在 KEY_ASSIGN 之前跑,把整段 AKIA... 替换。
            assert!(
                text.contains("[REDACTED:aws_key]"),
                "expected AWS marker in: {text}"
            );
            assert!(
                !text.contains("AKIAIOSFODNN7EXAMPLE"),
                "raw AWS key must not leak: {text}"
            );
        }
        other => panic!("expected Text block, got {other:?}"),
    }
}

#[tokio::test]
async fn queue_redacts_github_token() {
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "gh_leak".into(),
        payload: "Token: ghp_abcdefghijklmnopqrstuvwxyz0123456789",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "gh_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("[REDACTED:github_token]"), "text: {text}");
            assert!(!text.contains("abcdefghijklmnopqrstuvwxyz"), "text: {text}");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_redacts_bearer_token() {
    // 注意:此 payload 的 token 段 `eyJhbG...` 是合法 JWT,JWT pattern
    // 优先匹配(在 BEARER 之前),因此输出会是 `[REDACTED:jwt]`,
    // 而非 `Bearer [REDACTED]`。下面用一个不含 JWT 的普通 bearer
    // token 测试 BEARER pattern。
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "bearer_leak".into(),
        payload: "Authorization: Bearer abc123def456ghi789jkl012mno",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "bearer_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            // BEARER_TOKEN 把 token 整段换成 `Bearer [REDACTED]`(preserve 前缀)。
            assert!(text.contains("Bearer [REDACTED]"), "text: {text}");
            assert!(!text.contains("abc123def456ghi789"), "text: {text}");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_redacts_jwt_inside_bearer_header() {
    // 显式测试:JWT 形式的 bearer token 会被 JWT pattern 抢先命中,
    // 整段 token 替换为 `[REDACTED:jwt]`。
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "jwt_leak".into(),
        payload: "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxw",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "jwt_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("[REDACTED:jwt]"), "text: {text}");
            assert!(!text.contains("SflKxw"), "text: {text}");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_redacts_db_url() {
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "db_leak".into(),
        payload: "postgres://user:pass@localhost:5432/mydb",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "db_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            assert_eq!(text, "postgres://[REDACTED]");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_disabled_sanitizer_is_noop() {
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "aws_leak".into(),
        payload: "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::disabled()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "aws_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            assert_eq!(text, "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_error_path_text_payload_is_not_re_scanned() {
    // 工具返回 `is_error: true` 但仍带 text payload 含假 key。
    // queue 的 Ok(Err(_)) 与 timeout 分支用合成的 `ContentBlock::text(e.to_string())`
    // 替换 output.content —— 这里测试的是:is_error 但仍走 Ok(Ok(_)) 分支
    // (因为 Result 是 Ok,只是 output.is_error == true)。
    // 我们的代码对 Ok(Ok(_)) 一律 sanitize(无论 is_error),因此这个测试
    // 文档化「is_error: true 也走脱敏」,与错误分支不同(后者合成不同文本)。
    let leaky: Arc<dyn Tool> = Arc::new(ErrorLeakyTool {
        name: "error_leak".into(),
        payload: "API_KEY=secret",
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "error_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert!(out[0].is_error);
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            // 即便 is_error,Ok(Ok(_)) 分支仍走 sanitize。
            assert!(text.contains("[REDACTED]"), "text: {text}");
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn queue_nested_tool_result_is_recursively_sanitized() {
    let leaky: Arc<dyn Tool> = Arc::new(NestedLeakyTool);
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "nested_leaky".into(),
            args: serde_json::json!({}),
        }])
        .await;
    // 顶层是 ToolResult,递归 sanitize 应对内层 Text 做替换。
    assert_eq!(out.len(), 1);
    match &out[0].content[0] {
        ContentBlock::ToolResult { output, .. } => match &output.content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "API_KEY=[REDACTED]");
            }
            other => panic!("expected inner Text, got {other:?}"),
        },
        other => panic!("expected ToolResult block, got {other:?}"),
    }
}

#[tokio::test]
async fn post_tool_use_hook_sees_redacted_version() {
    let leaky: Arc<dyn Tool> = Arc::new(LeakyTool {
        name: "aws_leak".into(),
        payload: "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
    });
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (q, captured) =
        build_queue_with_hook(leaky, Arc::new(Sanitizer::with_defaults()), captured);
    let _ = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "aws_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    let texts = captured.lock().unwrap().clone();
    assert!(!texts.is_empty(), "hook should have captured something");
    let captured_text = &texts[0];
    assert!(
        captured_text.contains("[REDACTED:aws_key]"),
        "PostToolUse hook should see redacted version: {captured_text}"
    );
    assert!(
        !captured_text.contains("AKIAIOSFODNN7EXAMPLE"),
        "PostToolUse hook must NOT see raw key: {captured_text}"
    );
}

// ── Review 2026-06-30 P2-1 / P2-2:Timeout + PostToolUseFailure 分支测试 ──

/// `Ok(Err(ToolError::Execution))` 分支:工具返回 `Err` —— queue 不调
/// `sanitize_output`,而是合成 `ContentBlock::text(e.to_string())` 后
/// 直接 dispatch `PostToolUseFailure`。
///
/// Review 2026-06-30 P2-2 回归测试:确认该分支的合成错误文本不被二次
/// 扫描(虽然 `ToolError::Execution("AKIA...")` 含假密钥,也不应触发
/// sanitize —— 那会让"错误模板"误匹配,引入 false positive 与 CPU 浪费)。
struct ErrLeakyTool {
    name: String,
    err: ToolError,
}

#[async_trait]
impl Tool for ErrLeakyTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "tool that returns Err(ToolError)"
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
        Err(self.err.clone())
    }
}

/// Sleep 工具 —— 内部 sleep 超过 `ctx.timeout`,触发 queue 的 timeout 分支。
struct SleepTool {
    name: String,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "sleeps forever"
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
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        // 不会到达这里 —— sleep 会被 timeout 中断。
        unreachable!()
    }
}

#[tokio::test]
async fn queue_ok_err_branch_synthesizes_error_text_not_rescanned() {
    // P2-2:`Ok(Err(ToolError::Execution("AKIA...EXAMPLE")))` →
    // queue.rs:434-457 合成 `ContentBlock::text(e.to_string())`,**不调** sanitize。
    // 错误文本里碰巧含假 key 字面量(用户编的错误消息可能塞了上下文),但
    // 不应触发脱敏 —— 否则会产生"`ToolError::Display` 里有 AKIA 字面量
    // 也被脱敏"的反直觉行为。
    let leaky: Arc<dyn Tool> = Arc::new(ErrLeakyTool {
        name: "err_leak".into(),
        err: ToolError::Execution("simulated failure with API_KEY=hunter2 context".into()),
    });
    let q = build_queue_with(leaky, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "err_leak".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert!(out[0].is_error);
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            // 合成文本里含原始 "API_KEY=hunter2"(不应被脱敏)
            assert!(
                text.contains("API_KEY=hunter2"),
                "synthesized error text must NOT be re-scanned; got: {text}"
            );
            assert!(
                !text.contains("[REDACTED]"),
                "synthesized error text must NOT contain REDACTED marker; got: {text}"
            );
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn queue_timeout_branch_synthesizes_error_text_not_rescanned() {
    // P2-1:timeout 分支合成的 `ContentBlock::text("tool 'sleep_forever' timed out...")`
    // **不调** sanitize,与 queue.rs:458-485 一致。
    let leaky: Arc<dyn Tool> = Arc::new(SleepTool {
        name: "sleep_forever".into(),
    });
    // 用 100ms timeout —— 工具会 sleep 60s,必然超时。
    let mut ctx = make_ctx();
    ctx.timeout = std::time::Duration::from_millis(100);
    let reg = Arc::new(ToolRegistry::default());
    reg.register(leaky);
    let engine = Arc::new(HookEngine::new());
    let q =
        ToolExecutionQueue::with_sanitizer(reg, engine, ctx, Arc::new(Sanitizer::with_defaults()));
    let out = q
        .execute_all(vec![ToolCallRequest {
            id: "c1".into(),
            name: "sleep_forever".into(),
            args: serde_json::json!({}),
        }])
        .await;
    assert!(out[0].is_error, "timeout must produce is_error=true");
    match &out[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("timed out"),
                "synthesized timeout text must NOT be re-scanned; got: {text}"
            );
            assert!(
                !text.contains("[REDACTED]"),
                "timeout synthesis text must NOT contain REDACTED marker; got: {text}"
            );
        }
        _ => panic!("expected Text"),
    }
}
