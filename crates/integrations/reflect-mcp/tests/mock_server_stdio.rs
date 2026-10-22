//! reflect-mcp 集成测试 —— mock stdio MCP server 子进程 × 本 crate 的 client 全栈。
//!
//! 覆盖链路:子进程启动 → initialize 握手 → tools/list(echo + slow)
//! → tools/call(echo 原样回显 text)。
//!
//! ## 为什么这个文件必须存在(构建副作用,别删)
//!
//! cargo 在编译某包的**集成测试**时,会把该包的普通 bin 一并构建
//! (`CARGO_BIN_EXE_<name>` 指向它)。`reflect-exec` 的
//! `tests/mcp_integration.rs` 需要把 `mock_mcp_server` 拉起成真子进程,
//! 而 reflect-mcp 只是 reflect-exec 的 **dependency** —— dependency 只构建
//! lib,不构建 bin。因此没有本文件时,干净 checkout 上
//! `cargo test --workspace --all-targets` 不会产出
//! `target/debug/mock_mcp_server`,reflect-exec 的 MCP 子进程用例会直接
//! 失败("mock server should start")。
//!
//! 换句话说:本文件既是 mock server fixture 自身的回归测试,也是
//! 「让 mock_mcp_server 二进制随 `cargo test` 一起产出」的锚点。

use std::collections::HashMap;
use std::time::Duration;

use reflect_mcp::{McpConnectionManager, McpServerConfig, McpToolAdapter, McpTransport};
use reflect_tools::{Tool, ToolContext};

/// mock stdio server 配置:命令 = 本包的 mock_mcp_server bin。
fn mock_server_config() -> McpServerConfig {
    McpServerConfig {
        name: "mock".to_string(),
        transport: McpTransport::Stdio,
        command: Some(env!("CARGO_BIN_EXE_mock_mcp_server").to_string()),
        args: vec![],
        env: HashMap::new(),
        url: None,
        headers: HashMap::new(),
        timeout: Duration::from_secs(10),
    }
}

/// 真子进程:握手成功 + 列出 echo/slow 两个工具 + echo 回显。
#[tokio::test]
async fn mock_server_lists_tools_and_echo_roundtrips() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager
        .start_server(mock_server_config())
        .await
        .expect("mock server 应该启动成功");

    // 1) 工具列表:mock server 暴露 echo + slow。
    let names: Vec<&str> = handle.tools.iter().map(|d| d.full_name.as_str()).collect();
    assert_eq!(handle.tools.len(), 2, "expected echo + slow, got {names:?}");
    assert!(names.contains(&"mcp__mock__echo"), "got {names:?}");
    assert!(names.contains(&"mcp__mock__slow"), "got {names:?}");

    // 2) 生命周期事件:Started。
    let evt = rx.try_recv().expect("Started event");
    match evt {
        reflect_mcp::McpLifecycleEvent::Started { server, tools, .. } => {
            assert_eq!(server, "mock");
            assert_eq!(tools, 2);
        }
        other => panic!("expected Started, got {other:?}"),
    }

    // 3) tools/call:echo 原样回显 text。
    let desc = handle
        .tools
        .iter()
        .find(|d| d.original_name == "echo")
        .expect("echo 描述符");
    let adapter = McpToolAdapter::from_descriptor(
        handle.inner.clone(),
        desc,
        "mock",
        Duration::from_secs(10),
        None,
    );
    let out = adapter
        .execute(
            ToolContext::for_workspace(std::env::temp_dir()),
            serde_json::json!({"text": "ping"}),
        )
        .await
        .expect("echo 调用应成功");
    assert!(!out.is_error, "echo 不应报错: {out:?}");
    let text = match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(text.contains("ping"), "echo 应回显 text: {text:?}");

    manager.shutdown().await;
}
