//! v0.5 端到端集成测试 —— mock LSP server 子进程 + `LspConnectionManager` 全栈。
//!
//! 关键不变量:
//! - `bootstrap_lsp` 启动后,`ToolRegistry` 里有 `lsp` 工具(`LspTool`)
//! - 调 `lsp action=definition/references/hover` 走完 JSON-RPC
//! - 未匹配 server 的文件路径 → `ToolError::InvalidArgs`
//! - mock server 退出 → `LspLifecycleEvent::Failed`
//!
//! 镜像 `mcp_integration.rs` 的 `mock_binary_path()` helper 推算 binary 路径。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reflect_config::{LspFilePattern, LspServerEntry, LspServersSection, ReflectConfig};
use reflect_lsp::{LspConnectionManager, LspLifecycleEvent, LspTool};
use reflect_protocol::{Event, EventMsg, LspServerFailedEvent, LspServerStartedEvent};
use reflect_tools::{ToolContext, ToolRegistry, ToolSource};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn mock_binary_path() -> PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    target.join(profile).join("mock_lsp_server")
}

fn make_cfg(_name: &str, command: &str) -> LspServerEntry {
    LspServerEntry {
        command: command.to_string(),
        args: vec![],
        env: HashMap::new(),
        file_patterns: vec![LspFilePattern {
            glob: "**/*.rs".to_string(),
            language_id: "rust".to_string(),
        }],
        root_uri: None,
        initialization_options: None,
        timeout_ms: Some(5_000),
    }
}

fn make_reflect_config(entries: &[(&str, &str)]) -> ReflectConfig {
    let mut cfg = ReflectConfig::default();
    let mut servers = HashMap::new();
    for (name, cmd) in entries {
        servers.insert(name.to_string(), make_cfg(name, cmd));
    }
    cfg.lsp_servers = LspServersSection { servers };
    cfg
}

async fn start_manager(
    cfg: &ReflectConfig,
) -> (
    Arc<ToolRegistry>,
    Arc<LspConnectionManager>,
    mpsc::Receiver<Event>,
) {
    let tools = Arc::new(ToolRegistry::default());
    let (event_tx, event_rx) = mpsc::channel::<Event>(32);
    let (lsp_internal_tx, mut lsp_internal_rx) = mpsc::channel::<LspLifecycleEvent>(32);
    let manager = Arc::new(LspConnectionManager::new(lsp_internal_tx));

    // 内部事件 → protocol Event 转发
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        while let Some(evt) = lsp_internal_rx.recv().await {
            let msg = match evt {
                LspLifecycleEvent::Started {
                    server,
                    methods,
                    language_ids,
                } => EventMsg::LspServerStarted(LspServerStartedEvent {
                    server,
                    methods,
                    language_ids,
                }),
                LspLifecycleEvent::Failed {
                    server,
                    error,
                    will_retry,
                } => EventMsg::LspServerFailed(LspServerFailedEvent {
                    server,
                    error,
                    will_retry,
                }),
                LspLifecycleEvent::Stopped { .. } => continue,
            };
            if event_tx_clone
                .send(Event::new(reflect_protocol::EVENT_ID_NONE, msg))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 注册 LspTool
    let tool = Arc::new(LspTool::new(manager.clone()));
    tools.register_with_source(ToolSource::Runtime, tool);

    // 启动每个 server
    let shapes = cfg.lsp_server_configs().expect("valid config");
    for shape in shapes {
        let cfg: reflect_lsp::LspServerConfig = shape.try_into().expect("convert");
        let mgr = manager.clone();
        tokio::spawn(async move {
            let _ = mgr.start_server(cfg).await;
        });
    }
    // 等启动完成(server 启动后才有 handle)
    tokio::time::sleep(Duration::from_millis(500)).await;
    (tools, manager, event_rx)
}

fn ctx_for(workspace: &std::path::Path) -> ToolContext {
    ToolContext::for_workspace(workspace.to_path_buf())
}

#[tokio::test]
async fn lsp_tool_is_registered_with_runtime_source() {
    let tools = Arc::new(ToolRegistry::default());
    let (lsp_tx, _) = mpsc::channel::<LspLifecycleEvent>(1);
    let manager = Arc::new(LspConnectionManager::new(lsp_tx));
    let tool = Arc::new(LspTool::new(manager));
    tools.register_with_source(ToolSource::Runtime, tool);
    let t = tools.get("lsp").expect("lsp tool should be registered");
    assert_eq!(t.name(), "lsp");
}

#[tokio::test]
async fn lsp_definition_returns_location_via_mock() {
    if !mock_binary_path().exists() {
        eprintln!("mock_lsp_server binary not built; skipping");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\nfn bar() { foo(); }\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let out = timeout(
        Duration::from_secs(3),
        tool.execute(
            ctx,
            serde_json::json!({
                "action": "definition",
                "file_path": "a.rs",
                "line": 0,
                "character": 3
            }),
        ),
    )
    .await
    .expect("tool call timeout")
    .expect("tool call ok");
    assert!(!out.is_error, "is_error: {}", out.is_error);
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    // mock 返回 range 2:0..2:3
    assert!(
        text.contains("2") && text.contains("0") && text.contains("3"),
        "expected range 2:0..2:3 in response, got: {text}"
    );
}

#[tokio::test]
async fn lsp_references_returns_array() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\nfn bar() { foo(); }\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "references",
                "file_path": "a.rs",
                "line": 0,
                "character": 3
            }),
        )
        .await
        .expect("tool call ok");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    // mock 返回 2 个引用 → 序列化为 JSON 数组
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 2, "expected 2 references, got: {parsed}");
}

#[tokio::test]
async fn lsp_hover_returns_markdown() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "hover",
                "file_path": "a.rs",
                "line": 0,
                "character": 3
            }),
        )
        .await
        .expect("tool call ok");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    assert!(
        text.contains("mock hover"),
        "expected mock hover in: {text}"
    );
}

#[tokio::test]
async fn lsp_unsupported_file_path_returns_invalid_args() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("readme.md");
    std::fs::write(&md, "# hello").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let err = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "definition",
                "file_path": "readme.md",
                "line": 0,
                "character": 0
            }),
        )
        .await
        .unwrap_err();
    match err {
        reflect_protocol::ToolError::InvalidArgs { message } => {
            assert!(
                message.contains("no LSP server"),
                "expected 'no LSP server' in: {message}"
            );
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[tokio::test]
async fn lsp_unknown_action_errors() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    // "rename" 在 Phase A 未实现 → serde 反序列化失败
    let err = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "rename",
                "file_path": "a.rs",
                "line": 0,
                "character": 3,
                "new_name": "bar"
            }),
        )
        .await
        .unwrap_err();
    match err {
        reflect_protocol::ToolError::InvalidArgs { message } => {
            assert!(
                message.contains("invalid arguments") || message.contains("rename"),
                "got: {message}"
            );
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[tokio::test]
async fn lsp_bad_binary_emits_failed_event() {
    let cfg = make_reflect_config(&[("broken", "/this/binary/does/not/exist")]);
    let (_tools, _manager, mut event_rx) = start_manager(&cfg).await;
    // Failed event 应已被推。
    let mut found_failed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), event_rx.recv()).await {
            Ok(Some(Event {
                msg: EventMsg::LspServerFailed(e),
                ..
            })) => {
                assert_eq!(e.server, "broken");
                assert!(!e.will_retry);
                found_failed = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(found_failed, "expected LspServerFailed event");
}

// ── Phase B1: documentSymbol / completion / signatureHelp ─────────────

#[tokio::test]
async fn lsp_document_symbol_returns_nested() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\nlet bar = 1;\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    // documentSymbol 不需要 line/character。
    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "document_symbol",
                "file_path": "a.rs"
            }),
        )
        .await
        .expect("tool call ok");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    // mock 返回 2 个 nested DocumentSymbol —— serde JSON 序列化为
    // `[[DocumentSymbol, DocumentSymbol]]`(`DocumentSymbolResponse` 的 Nested 变体)。
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let outer = parsed.as_array().expect("outer array");
    assert_eq!(outer.len(), 2, "expected 2 symbols, got: {parsed}");
    let first = &outer[0];
    assert_eq!(first["name"], "foo");
    assert_eq!(first["kind"], 12); // Function
}

#[tokio::test]
async fn lsp_completion_returns_list() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo() {}\nfn bar() {}\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "completion",
                "file_path": "a.rs",
                "line": 0,
                "character": 0
            }),
        )
        .await
        .expect("tool call ok");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    // mock 返回 `CompletionList { isIncomplete: false, items: [...] }`。
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    assert_eq!(parsed["isIncomplete"], false);
    let items = parsed["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "expected 2 completions, got: {parsed}");
    assert_eq!(items[0]["label"], "foo");
    assert_eq!(items[1]["label"], "bar");
}

#[tokio::test]
async fn lsp_signature_help_returns_signatures() {
    if !mock_binary_path().exists() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let a_rs = tmp.path().join("a.rs");
    std::fs::write(&a_rs, "fn foo(a: i32, b: &str) {}\n").unwrap();

    let cfg = make_reflect_config(&[("mock", &mock_binary_path().to_string_lossy())]);
    let (tools, _manager, _rx) = start_manager(&cfg).await;
    let tool = tools.get("lsp").expect("lsp tool");
    let ctx = ctx_for(tmp.path());
    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "action": "signature_help",
                "file_path": "a.rs",
                "line": 0,
                "character": 7
            }),
        )
        .await
        .expect("tool call ok");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse json");
    let sigs = parsed["signatures"].as_array().expect("signatures array");
    assert_eq!(sigs.len(), 1, "expected 1 signature");
    assert_eq!(sigs[0]["label"], "fn foo(a: i32, b: &str)");
    let params = sigs[0]["parameters"].as_array().expect("parameters");
    assert_eq!(params.len(), 2, "expected 2 parameters");
}

// ── Phase B1: per-action permission 表 ───────────────────────────────

#[tokio::test]
async fn lsp_tool_required_permission_is_prompt_fallback() {
    // tool-level fallback 仍是 Prompt(未知 action 走 prompt 拒绝);
    // 但 execute 路径上只读 action per-action Auto,LLM 不会被反复 confirm。
    let tools = Arc::new(ToolRegistry::default());
    let (lsp_tx, _) = mpsc::channel::<LspLifecycleEvent>(1);
    let manager = Arc::new(LspConnectionManager::new(lsp_tx));
    let tool = Arc::new(LspTool::new(manager));
    tools.register_with_source(ToolSource::Runtime, tool);
    let t = tools.get("lsp").expect("lsp tool");
    assert_eq!(
        t.required_permission(),
        reflect_protocol::PermissionMode::Prompt
    );
    // 6 个只读 action 全部 Auto。
    use reflect_lsp::LspAction;
    for action in [
        LspAction::Definition,
        LspAction::References,
        LspAction::Hover,
        LspAction::DocumentSymbol,
        LspAction::Completion,
        LspAction::SignatureHelp,
    ] {
        assert_eq!(
            action.required_permission(),
            reflect_protocol::PermissionMode::Auto,
            "{action:?} should be Auto (Phase B1)"
        );
        assert!(
            action.is_concurrency_safe(),
            "{action:?} should be concurrent"
        );
    }
}
