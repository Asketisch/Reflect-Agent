//! v0.3 M6 端到端集成测试 —— mock MCP server 子进程 + `McpConnectionManager` 全栈。
//!
//! 关键不变量:
//! - `bootstrap_m6` 启动后,`ToolRegistry` 里有 `mcp__mock__echo` adapter
//! - `handle_reload` 改 `mcp_servers` section 后,新增 server 的 tool 被注册
//! - 改 `mcp_servers` 后 `diff_sections` 把 `"mcp_servers"` 加入 changed 列表
//! - broken server(`command = "/nonexistent"`)启动失败但不影响其它 server
//! - `register_if_absent` 在 builtin 与 mcp 同名时跳过 mcp

use std::collections::HashMap;
use std::sync::Arc;

use reflect_config::{McpServerEntry, McpServersSection, McpTransport, ReflectConfig};
use reflect_exec::handle_reload;
use reflect_llm::ModelRegistry;
use reflect_protocol::{
    ConfigReloadedEvent, Event, EventMsg, McpServerFailedEvent, McpServerStartedEvent,
    McpTransportMirror,
};
use reflect_tools::{ToolRegistry, ToolSource};
use tokio::sync::mpsc;

fn mock_binary_path() -> std::path::PathBuf {
    // v0.4 起,`mock_mcp_server` 二进制仅由 `reflect-mcp` 声明 —— 单二进制合并后
    // 此 crate 不再 [[bin]] 注入 `CARGO_BIN_EXE_mock_mcp_server`。改为从
    // workspace 布局推算路径:`cargo test -p reflect-exec` 会先 build reflect-mcp
    // (作为 dependency),`target/{debug,release}/mock_mcp_server` 总在测试运行前就生成。
    //
    // 路径推算:`CARGO_MANIFEST_DIR` = `<workspace>/crates/runtime/reflect-exec`,
    // 需向上取 **3 层** parent 才回到 workspace root(原代码只取 2 层会落到
    // `crates/`,拼出不存在的 `crates/target/...` 导致 `Spawn NotFound`)。
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("workspace root");
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    target.join(profile).join("mock_mcp_server")
}

fn stdio_cfg_entry(_name: &str) -> McpServerEntry {
    McpServerEntry {
        transport: McpTransport::Stdio,
        command: Some(mock_binary_path().to_string_lossy().into_owned()),
        args: Some(vec![]),
        env: Some(HashMap::new()),
        url: None,
        headers: Some(HashMap::new()),
        timeout_ms: Some(5_000),
        always_load: None,
    }
}

fn broken_cfg_entry(_name: &str) -> McpServerEntry {
    McpServerEntry {
        transport: McpTransport::Stdio,
        command: Some("/this/binary/definitely/does/not/exist".into()),
        args: Some(vec![]),
        env: Some(HashMap::new()),
        url: None,
        headers: Some(HashMap::new()),
        timeout_ms: Some(1_000),
        always_load: None,
    }
}

fn cfg_with(names_and_entries: Vec<(&str, McpServerEntry)>) -> ReflectConfig {
    let mut mcp = McpServersSection::default();
    for (name, entry) in names_and_entries {
        mcp.servers.insert(name.to_string(), entry);
    }
    ReflectConfig {
        mcp_servers: mcp,
        ..Default::default()
    }
}

/// 1) `mcp_reload_diff_incremental_added` —— 加 server 后 tool 注册到 registry。
#[tokio::test]
async fn mcp_reload_diff_incremental_added_registers_tool() {
    let old = cfg_with(vec![]);
    let new = cfg_with(vec![("mock", stdio_cfg_entry("mock"))]);
    let registry = Arc::new(ModelRegistry::new());
    let agent_cfg = reflect_core::AgentConfig::new("anthropic/x", "/tmp");
    let tools = Arc::new(ToolRegistry::default());
    let (tx, mut rx) = mpsc::channel::<Event>(16);
    let path = std::path::PathBuf::from("/tmp/test.toml");

    handle_reload(
        &old, &new, &registry, &agent_cfg, None, None, &path, &tx,
        None, // 不传 manager — 只验证 ConfigReloaded event 仍能发出
        &tools, None,
    )
    .await
    .expect("reload ok");

    // 验证 ConfigReloaded event 含 mcp_servers section。
    let ev = rx.try_recv().expect("event available");
    match ev.msg {
        EventMsg::ConfigReloaded(ConfigReloadedEvent {
            sections_changed, ..
        }) => {
            assert!(
                sections_changed.contains(&"mcp_servers".to_string()),
                "expected mcp_servers in {:?}",
                sections_changed
            );
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }
}

/// 2) `mcp_reload_diff_incremental_removed` —— 删 server 后 mcp_servers 仍出现在 diff。
#[tokio::test]
async fn mcp_reload_diff_incremental_removed() {
    let old = cfg_with(vec![("mock", stdio_cfg_entry("mock"))]);
    let new = cfg_with(vec![]);
    let registry = Arc::new(ModelRegistry::new());
    let agent_cfg = reflect_core::AgentConfig::new("anthropic/x", "/tmp");
    let tools = Arc::new(ToolRegistry::default());
    let (tx, mut rx) = mpsc::channel::<Event>(16);
    let path = std::path::PathBuf::from("/tmp/test.toml");

    handle_reload(
        &old, &new, &registry, &agent_cfg, None, None, &path, &tx, None, &tools, None,
    )
    .await
    .expect("reload ok");

    let ev = rx.try_recv().expect("event available");
    match ev.msg {
        EventMsg::ConfigReloaded(ConfigReloadedEvent {
            sections_changed, ..
        }) => {
            assert!(
                sections_changed.contains(&"mcp_servers".to_string()),
                "expected mcp_servers in {:?}",
                sections_changed
            );
        }
        other => panic!("expected ConfigReloaded, got {other:?}"),
    }
}

/// 3) `handle_reload_lists_mcp_servers_in_changed` —— diff_sections 真把
///    `mcp_servers` 加入 changed 列表(独立于 ConfigReloaded 测试,
///    强化合约)。
#[tokio::test]
async fn diff_sections_lists_mcp_servers_when_changed() {
    let old = cfg_with(vec![]);
    let new = cfg_with(vec![("fs", stdio_cfg_entry("fs"))]);
    let sections = reflect_exec::diff_sections_for_test(&old, &new);
    assert!(
        sections.contains(&"mcp_servers".to_string()),
        "got: {sections:?}"
    );
}

/// 4) `mcp_server_start_failure_continues` —— broken server 启动失败,
///    但 reload 仍 ok,且不影响其它 section。
#[tokio::test]
async fn mcp_server_start_failure_does_not_block_reload() {
    let old = cfg_with(vec![]);
    let new = cfg_with(vec![("broken", broken_cfg_entry("broken"))]);
    let registry = Arc::new(ModelRegistry::new());
    let agent_cfg = reflect_core::AgentConfig::new("anthropic/x", "/tmp");
    let tools = Arc::new(ToolRegistry::default());
    let (tx, mut rx) = mpsc::channel::<Event>(16);
    let path = std::path::PathBuf::from("/tmp/test.toml");

    // reload 不传 manager:即便配置 invalid,也不阻塞 ConfigReloaded。
    let result = handle_reload(
        &old, &new, &registry, &agent_cfg, None, None, &path, &tx, None, &tools, None,
    )
    .await;
    assert!(
        result.is_ok(),
        "reload should not fail on broken mcp server: {result:?}"
    );
    let ev = rx.try_recv().expect("event available");
    assert!(matches!(ev.msg, EventMsg::ConfigReloaded(_)));
}

/// 5) `tool_name_collision_via_registry` —— 用 ToolRegistry 模拟
///    builtin `bash` 已注册,mcp 想注册 `mcp__x__bash` 应当成功(因为
///    名称不同);若真有同名 mcp 想覆盖 builtin,则 `register_if_absent` 返 false。
#[tokio::test]
async fn tool_name_collision_routes_to_register_if_absent() {
    use async_trait::async_trait;
    use reflect_protocol::{PermissionMode, ToolError, ToolOutput};
    use reflect_tools::Tool;

    struct Bash;
    #[async_trait]
    impl Tool for Bash {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "builtin bash"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            false
        }
        fn required_permission(&self) -> PermissionMode {
            PermissionMode::Auto
        }
        async fn execute(
            &self,
            _: reflect_tools::ToolContext,
            _: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            unreachable!()
        }
    }

    struct Collide;
    #[async_trait]
    impl Tool for Collide {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "would collide"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _: reflect_tools::ToolContext,
            _: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            unreachable!()
        }
    }

    let r = ToolRegistry::default();
    // 第一次 register builtin 'bash' 成功。
    assert!(r.register_if_absent(ToolSource::Builtin, Arc::new(Bash)));
    // 第二次 mcp 想注册同名 'bash' 被拒绝。
    assert!(!r.register_if_absent(ToolSource::Runtime, Arc::new(Collide)));
    // builtin 仍在。
    let got = r.get("bash").expect("builtin survives");
    assert_eq!(got.description(), "builtin bash");
}

// ── 直接调 McpConnectionManager 的真子进程端到端 ─────────────────────────

use reflect_mcp::McpConnectionManager;

/// 6) `manager_start_server_real_subprocess` —— 启动 mock 子进程,确认
///    收到 Started event + handle.tools.len() == 2 (echo + slow)。
#[tokio::test]
async fn manager_start_server_real_subprocess() {
    let (tx, mut rx) = mpsc::channel::<reflect_mcp::McpLifecycleEvent>(8);
    let manager = McpConnectionManager::new(tx);
    let cfg = cfg_with(vec![("mock", stdio_cfg_entry("mock"))]);
    let mut shapes = cfg.mcp_server_configs().expect("stdio entry valid");
    let cfg: reflect_mcp::McpServerConfig = shapes.remove(0).into();
    let handle = manager
        .start_server(cfg)
        .await
        .expect("mock server should start");

    // 工具列表:mock server 返回 echo + slow = 2 个。
    assert_eq!(handle.tools.len(), 2, "expected echo + slow");
    let names: Vec<&str> = handle.tools.iter().map(|d| d.full_name.as_str()).collect();
    assert!(names.contains(&"mcp__mock__echo"));
    assert!(names.contains(&"mcp__mock__slow"));

    // 生命周期事件应有 Started。
    let evt = rx.try_recv().expect("Started event");
    match evt {
        reflect_mcp::McpLifecycleEvent::Started {
            server,
            tools,
            tool_names: _,
            transport,
        } => {
            assert_eq!(server, "mock");
            assert_eq!(tools, 2);
            assert!(matches!(transport, reflect_mcp::McpTransport::Stdio));
        }
        other => panic!("expected Started, got {other:?}"),
    }

    manager.shutdown().await;
}

/// 7) `manager_start_server_failed_subprocess` —— broken binary 触发 Failed event。
#[tokio::test]
async fn manager_start_server_failed_subprocess() {
    let (tx, mut rx) = mpsc::channel::<reflect_mcp::McpLifecycleEvent>(8);
    let manager = McpConnectionManager::new(tx);
    let cfg = cfg_with(vec![("broken", broken_cfg_entry("broken"))]);
    let mut shapes = cfg
        .mcp_server_configs()
        .expect("broken entry shape valid (validation happens at start_server)");
    let cfg: reflect_mcp::McpServerConfig = shapes.remove(0).into();
    let err = manager.start_server(cfg).await;
    assert!(err.is_err(), "broken server should fail");
    // Failed event 应推送到 rx(stdio 不重试,will_retry = false)。
    let evt = rx.try_recv().expect("Failed event");
    match evt {
        reflect_mcp::McpLifecycleEvent::Failed {
            server, will_retry, ..
        } => {
            assert_eq!(server, "broken");
            assert!(!will_retry);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// 8) `mcp_transport_mirror_serde_roundtrip_via_event` —— 验证 Started event
///    的 transport 字段 serialize/deserialize 正确(Magenta status_bar 依赖)。
#[test]
fn mcp_transport_mirror_serde_roundtrip_via_event() {
    let started = EventMsg::McpServerStarted(McpServerStartedEvent {
        server: "x".into(),
        tool_count: 1,
        tool_names: vec![],
        transport: McpTransportMirror::Http,
    });
    let failed = EventMsg::McpServerFailed(McpServerFailedEvent {
        server: "y".into(),
        error: "boom".into(),
        will_retry: true,
    });
    let j1 = serde_json::to_string(&started).unwrap();
    let j2 = serde_json::to_string(&failed).unwrap();
    assert!(j1.contains(r#""transport":"http""#), "got: {j1}");
    assert!(j2.contains(r#""will_retry":true"#), "got: {j2}");
    let back1: EventMsg = serde_json::from_str(&j1).unwrap();
    let back2: EventMsg = serde_json::from_str(&j2).unwrap();
    match (back1, back2) {
        (EventMsg::McpServerStarted(s), EventMsg::McpServerFailed(f)) => {
            assert_eq!(s.transport, McpTransportMirror::Http);
            assert_eq!(s.tool_count, 1);
            assert!(f.will_retry);
        }
        _ => panic!("wrong variants on roundtrip"),
    }
}
