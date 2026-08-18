//! Mock MCP stdio server —— 仅用于 reflect-mcp 自身测试。
//!
//! 协议版本 `2025-06-18`,与 `reflect_mcp::PROTOCOL_VERSION` 对齐。
//! 支持 2 个 tool:`echo`(立即返回)和 `slow`(sleep 2s 用于超时测试)。
//!
//! ## 输入输出格式
//!
//! JSON-RPC 2.0 over stdio(line-delimited JSON,每行一条 message)。
//! 不实现 notifications 的异步 push(rmcp 1.7 client 不强制要求)。
//!
//! ## 启动方式
//!
//! ```toml
//! [mcp_servers.mock]
//! type = "stdio"
//! command = "/path/to/reflect-mcp-mock_mcp_server"
//! ```
//!
//! ## env
//!
//! `MOCK_GREETING` —— 设置后 `echo` 工具返回 `"{greeting}: {text}"`,
//! 用于测试 env 变量变更后 reload 是否生效。

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, Write};
use std::thread::sleep;
use std::time::Duration;

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut handles = HashMap::new();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                });
                writeln!(stdout, "{err}").ok();
                stdout.flush().ok();
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        // 通知类型 (无 id 且无 result 期望): 直接吞掉,不响应。
        if msg.get("id").is_none() {
            // 只有在完全没有 "id" 字段时才视作通知。
            continue;
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = handle_request(method, &params, &mut handles);
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": response,
        });
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

fn handle_request(method: &str, params: &Value, _handles: &mut HashMap<String, Value>) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "reflect-mock",
                "version": "0.3.0",
            },
            "capabilities": {
                "tools": {"listChanged": false},
            },
        }),
        "tools/list" => json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "返回传入的 text 字段,可在 env MOCK_GREETING 启用时加前缀",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                        },
                        "required": ["text"],
                    },
                },
                {
                    "name": "slow",
                    "description": "sleep 2s 后返回 'slept',用于 timeout 测试",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                    },
                },
            ],
        }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            match name {
                "echo" => {
                    let text = args
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let greeting = env::var("MOCK_GREETING").unwrap_or_default();
                    let out = if greeting.is_empty() {
                        text
                    } else {
                        format!("{greeting}: {text}")
                    };
                    json!({
                        "content": [{"type": "text", "text": out}],
                        "isError": false,
                    })
                }
                "slow" => {
                    sleep(Duration::from_secs(2));
                    json!({
                        "content": [{"type": "text", "text": "slept"}],
                        "isError": false,
                    })
                }
                other => json!({
                    "content": [{"type": "text", "text": format!("unknown tool: {other}")}],
                    "isError": true,
                }),
            }
        }
        "ping" => json!({}),
        // 其他方法 (resources/list, prompts/list, ...) 留空。
        _ => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {method}")}],
            "isError": true,
        }),
    }
}
