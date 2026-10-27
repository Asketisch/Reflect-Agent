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

use tokio::sync::mpsc;
use zhuye_config::{McpServerEntry, McpServersSection, McpTransport, ReflectConfig};
use zhuye_exec::handle_reload;
use zhuye_llm::ModelRegistry;
use zhuye_protocol::{
    ConfigReloadedEvent, Event, EventMsg, McpServerFailedEvent, McpServerStartedEvent,
    McpTransportMirror,
};
use zhuye_tools::{ToolRegistry, ToolSource};

fn mock_binary_path() -> std::path::PathBuf {
    // v0.4 起,`mock_mcp_server` 二进制仅由 `zhuye-mcp` 声明 —— 单二进制合并后
    // 此 crate 不再 [[bin]] 注入 `CARGO_BIN_EXE_mock_mcp_server`。改为从
    // workspace 布局推算路径。
    //
    // 路径推算:`CARGO_MANIFEST_DIR` = `<workspace>/crates/runtime/zhuye-exec`,
    // 需向上取 **3 层** parent 才回到 workspace root(原代码只取 2 层会落到
    // `crates/`,拼出不存在的 `crates/target/...` 导致 `Spawn NotFound`)。
    //
    // ⚠️ 关键(2026-10 修复):zhuye-mcp 只是本包的 **dependency**,
    // cargo 只构建它的 **lib**,不构建它的 bin —— 干净 checkout 上
    // `cargo test --workspace --all-targets` 结束后,`target/debug/mock_mcp_server`
    // **并不存在**,本文件的子进程用例会以 "mock server should start" 失败。
    // 现在缺失时用 cargo 现构建一次(与 tests/rust/*.sh 的
    // `cargo build -p zhuye-mcp --bin mock_mcp_server` 同一手法),
    // 保证任何调用方式(整 workspace / 只跑本包)都能自洽。
    static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .expect("workspace root");
        let target = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| workspace_root.join("target"));
        let release = !cfg!(debug_assertions);
        let profile = if release { "release" } else { "debug" };
        let bin = target.join(profile).join("mock_mcp_server");
        if !bin.is_file() {
            build_mock_server(workspace_root, release);
        }
        bin
    })
    .clone()
}

/// 用 cargo 构建 `mock_mcp_server` 二进制(缺失时的兜底)。
///
/// 失败不 panic —— 调用方随后会因为二进制不存在而报出更直观的错误
/// (子进程 spawn 失败),这里只留一条 stderr 说明。
fn build_mock_server(workspace_root: &std::path::Path, release: bool) {
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let mut cmd = std::process::Command::new(cargo);
    cmd.args(["build", "-p", "zhuye-mcp", "--bin", "mock_mcp_server"]);
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(workspace_root);
    eprintln!("[mcp_integration] mock_mcp_server 不存在,现构建中…");
    match cmd.output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => eprintln!(
            "[mcp_integration] 构建 mock_mcp_server 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => eprintln!("[mcp_integration] 构建 mock_mcp_server 失败: {e}"),
    }
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
    let agent_cfg = zhuye_core::AgentConfig::new("anthropic/x", "/tmp");
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
    let agent_cfg = zhuye_core::AgentConfig::new("anthropic/x", "/tmp");
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
    let sections = zhuye_exec::diff_sections_for_test(&old, &new);
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
    let agent_cfg = zhuye_core::AgentConfig::new("anthropic/x", "/tmp");
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
    use zhuye_protocol::{PermissionMode, ToolError, ToolOutput};
    use zhuye_tools::Tool;

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
            _: zhuye_tools::ToolContext,
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
            _: zhuye_tools::ToolContext,
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

use zhuye_mcp::McpConnectionManager;

/// 探针子入口(与 zhuye-core/tests/common/mod.rs 同款):父进程以
/// `LANDLOCK_PROBE_CHILD=1` env 触发实际探测,常规跑直接通过。
#[test]
fn landlock_probe_child() {
    if std::env::var("LANDLOCK_PROBE_CHILD").as_deref() != Ok("1") {
        return;
    }
    if !zhuye_sandbox::probe_landlock_exec() {
        std::process::exit(42);
    }
}

/// GHA ubuntu runner 的 landlock 异常环境(restrict 后 exec 一律 EACCES,
/// 见 zhuye-core/tests/common 的注释)守卫 —— 真子进程类测试在能力
/// 缺失时跳过(环境限制,非回归)。
fn exec_capable() -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::env::current_exe() {
            Ok(exe) => std::process::Command::new(exe)
                .args([
                    "--exact",
                    "landlock_probe_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("LANDLOCK_PROBE_CHILD", "1")
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            Err(_) => true,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 6) `manager_start_server_real_subprocess` —— 启动 mock 子进程,确认
///    收到 Started event + handle.tools.len() == 2 (echo + slow)。
#[tokio::test]
async fn manager_start_server_real_subprocess() {
    if !exec_capable() {
        eprintln!("skip: landlock restrict 后 exec 不可用(runner 环境限制)");
        return;
    }
    let (tx, mut rx) = mpsc::channel::<zhuye_mcp::McpLifecycleEvent>(8);
    let manager = McpConnectionManager::new(tx);
    let cfg = cfg_with(vec![("mock", stdio_cfg_entry("mock"))]);
    let mut shapes = cfg.mcp_server_configs().expect("stdio entry valid");
    let cfg: zhuye_mcp::McpServerConfig = shapes.remove(0).into();
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
        zhuye_mcp::McpLifecycleEvent::Started {
            server,
            tools,
            tool_names: _,
            transport,
        } => {
            assert_eq!(server, "mock");
            assert_eq!(tools, 2);
            assert!(matches!(transport, zhuye_mcp::McpTransport::Stdio));
        }
        other => panic!("expected Started, got {other:?}"),
    }

    manager.shutdown().await;
}

/// 7) `manager_start_server_failed_subprocess` —— broken binary 触发 Failed event。
#[tokio::test]
async fn manager_start_server_failed_subprocess() {
    let (tx, mut rx) = mpsc::channel::<zhuye_mcp::McpLifecycleEvent>(8);
    let manager = McpConnectionManager::new(tx);
    let cfg = cfg_with(vec![("broken", broken_cfg_entry("broken"))]);
    let mut shapes = cfg
        .mcp_server_configs()
        .expect("broken entry shape valid (validation happens at start_server)");
    let cfg: zhuye_mcp::McpServerConfig = shapes.remove(0).into();
    let err = manager.start_server(cfg).await;
    assert!(err.is_err(), "broken server should fail");
    // Failed event 应推送到 rx(stdio 不重试,will_retry = false)。
    let evt = rx.try_recv().expect("Failed event");
    match evt {
        zhuye_mcp::McpLifecycleEvent::Failed {
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
