//! Mock LSP stdio server —— 仅用于 `reflect-lsp` 自身测试。
//!
//! 镜像 `reflect-mcp/src/bin/mock_mcp_server.rs` 的极简风格:实现 LSP
//! 协议子集,`initialize` / `textDocument/didOpen` / `didClose` /
//! `definition` / `references` / `hover` / `shutdown` / `exit`,
//! 其他 method 返回 JSON-RPC 错误 `-32601 MethodNotFound`。
//!
//! ## 输入输出格式
//!
//! LSP 规范:`Content-Length: N\r\n\r\n` 头 + 紧跟 N 字节 UTF-8 body,
//! 一次 message 一个 frame(本 mock 与 `transport.rs` 严格对齐)。
//!
//! ## 行为
//!
//! - `textDocument/didOpen` 存到 `HashMap<Uri, String>`。
//! - `textDocument/definition` —— 在 text 里找 `foo` 第一次出现,返回
//!   `Location { uri, range: 2:0..2:3 }`(line 2 第 0-3 字符,模拟定义行)。
//! - `textDocument/references` —— 返回 2 个 Location(模拟多个引用)。
//! - `textDocument/hover` —— 返回 Markdown 格式 hover。
//! - 其他 method → JSON-RPC 错误 `code = -32601`。

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    // 累积 stdin 字节,逐次切 frame。
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut handle = stdin.lock();
    let mut chunk = [0u8; 4096];
    loop {
        match handle.read(&mut chunk) {
            Ok(0) => break, // EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // 切 frame 直到 buffer 不完整。
                loop {
                    match try_parse_frame(&mut buf) {
                        FrameResult::NeedMore => break,
                        FrameResult::Done(value) => {
                            // 派发:有 id → 响应,无 id → notification 静默吞。
                            let id = value.get("id").cloned();
                            if let Some(id) = id {
                                let method =
                                    value.get("method").and_then(Value::as_str).unwrap_or("");
                                let params = value.get("params").cloned().unwrap_or(Value::Null);
                                let response = handle_request(method, &params, &mut docs);
                                let resp = if let Some(err) = response.get("__error") {
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "error": { "code": -32601, "message": err.as_str().unwrap_or("") }
                                    })
                                } else {
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": response
                                    })
                                };
                                write_frame(&mut stdout, &resp);
                            }
                            // notification 直接吞
                        }
                        FrameResult::Error(msg) => {
                            eprintln!("mock_lsp_server: bad frame: {msg}");
                            // 跳过首字节避免死循环
                            if !buf.is_empty() {
                                buf.remove(0);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("mock_lsp_server: read error: {e}");
                break;
            }
        }
    }
}

// ── framing(镜像 reflect-lsp::transport::framing) ─────────────────────

enum FrameResult {
    NeedMore,
    Done(Value),
    Error(String),
}

fn try_parse_frame(buf: &mut Vec<u8>) -> FrameResult {
    const SEP: &[u8] = b"\r\n\r\n";
    let Some(sep_pos) = buf.windows(SEP.len()).position(|w| w == SEP) else {
        return FrameResult::NeedMore;
    };
    let header_str = match std::str::from_utf8(&buf[..sep_pos]) {
        Ok(s) => s,
        Err(e) => return FrameResult::Error(format!("non-utf8 header: {e}")),
    };
    let content_length = match parse_content_length(header_str) {
        Some(n) => n,
        None => return FrameResult::Error(format!("missing Content-Length: {header_str:?}")),
    };
    let body_start = sep_pos + SEP.len();
    let body_end = body_start + content_length;
    if buf.len() < body_end {
        return FrameResult::NeedMore;
    }
    let value: Value = match serde_json::from_slice(&buf[body_start..body_end]) {
        Ok(v) => v,
        Err(e) => return FrameResult::Error(format!("invalid JSON: {e}")),
    };
    buf.drain(..body_end);
    FrameResult::Done(value)
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn write_frame(stdout: &mut io::Stdout, msg: &Value) {
    let body = serde_json::to_vec(msg).expect("encode json");
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = stdout.lock();
    out.write_all(header.as_bytes()).ok();
    out.write_all(&body).ok();
    out.flush().ok();
}

// ── request 派发 ──────────────────────────────────────────────────────

fn handle_request(method: &str, params: &Value, docs: &mut HashMap<String, String>) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "serverInfo": { "name": "reflect-mock-lsp", "version": "0.5.0" },
            "capabilities": {
                "definitionProvider": true,
                "referencesProvider": true,
                "hoverProvider": true,
                "documentSymbolProvider": true,
                "completionProvider": { "triggerCharacters": [".", ":"] },
                "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
                "textDocumentSync": { "openClose": true, "change": 0, "willSave": false, "save": false }
            }
        }),
        "shutdown" => json!(null),
        "exit" => {
            // 立即退出,绕过进一步 read。
            std::process::exit(0);
        }
        "textDocument/didOpen" => {
            if let Some(td) = params.get("textDocument")
                && let (Some(uri), Some(text)) = (
                    td.get("uri").and_then(Value::as_str),
                    td.get("text").and_then(Value::as_str),
                )
            {
                docs.insert(uri.to_string(), text.to_string());
            }
            json!(null)
        }
        "textDocument/didClose" => {
            if let Some(td) = params.get("textDocument")
                && let Some(uri) = td.get("uri").and_then(Value::as_str)
            {
                docs.remove(uri);
            }
            json!(null)
        }
        "textDocument/definition" => {
            // 找 text 里的 "foo" 第一次出现 → 模拟返回 2:0..2:3。
            let uri = params
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = docs.get(uri).cloned().unwrap_or_default();
            let _ = text; // 这里不需要用 text 的精确位置,固定返回 2:0..2:3。
            json!({
                "uri": uri,
                "range": {
                    "start": { "line": 2, "character": 0 },
                    "end":   { "line": 2, "character": 3 }
                }
            })
        }
        "textDocument/references" => {
            // 返回 2 个引用位置。
            let uri = params
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("");
            json!([
                {
                    "uri": uri,
                    "range": {
                        "start": { "line": 5, "character": 4 },
                        "end":   { "line": 5, "character": 7 }
                    }
                },
                {
                    "uri": uri,
                    "range": {
                        "start": { "line": 8, "character": 10 },
                        "end":   { "line": 8, "character": 13 }
                    }
                }
            ])
        }
        "textDocument/hover" => json!({
            "contents": {
                "kind": "markdown",
                "value": "**foo** — mock hover"
            },
            "range": {
                "start": { "line": 1, "character": 0 },
                "end":   { "line": 1, "character": 3 }
            }
        }),
        // Phase B1:文件大纲 —— 返回 2 个 nested symbol(1 个 function + 1 个 variable)。
        "textDocument/documentSymbol" => json!([
            {
                "name": "foo",
                "kind": 12, // Function
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end":   { "line": 0, "character": 11 }
                },
                "selectionRange": {
                    "start": { "line": 0, "character": 3 },
                    "end":   { "line": 0, "character": 6 }
                },
                "children": []
            },
            {
                "name": "bar",
                "kind": 13, // Variable
                "range": {
                    "start": { "line": 1, "character": 0 },
                    "end":   { "line": 1, "character": 14 }
                },
                "selectionRange": {
                    "start": { "line": 1, "character": 4 },
                    "end":   { "line": 1, "character": 7 }
                },
                "children": []
            }
        ]),
        // Phase B1:补全 —— 返回 2 个 CompletionItem 数组。
        "textDocument/completion" => json!({
            "isIncomplete": false,
            "items": [
                {
                    "label": "foo",
                    "kind": 6, // Function
                    "detail": "fn foo()",
                    "insertText": "foo()"
                },
                {
                    "label": "bar",
                    "kind": 6,
                    "detail": "fn bar()",
                    "insertText": "bar()"
                }
            ]
        }),
        // Phase B1:signature help —— 返回 1 个 signature 包含 2 个 parameter。
        "textDocument/signatureHelp" => json!({
            "signatures": [
                {
                    "label": "fn foo(a: i32, b: &str)",
                    "documentation": { "kind": "markdown", "value": "Mock signature help" },
                    "parameters": [
                        { "label": "a: i32", "documentation": { "kind": "plaintext", "value": "first arg" } },
                        { "label": "b: &str", "documentation": { "kind": "plaintext", "value": "second arg" } }
                    ]
                }
            ],
            "activeSignature": 0,
            "activeParameter": 0
        }),
        // 未实现 method → 返回 __error tag 触发 mock 调用方返回 -32601。
        _ => json!({ "__error": format!("Method not found: {method}") }),
    }
}
