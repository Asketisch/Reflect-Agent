//! `reflect-lsp` — LSP (Language Server Protocol) client integration (v0.5)。
//!
//! 把每个 LSP server 暴露的 language capability(`textDocument/definition`、
//! `textDocument/references`、`textDocument/hover` 等)经 JSON-RPC over stdio
//! 与外部子进程通信,统一通过单一 `lsp` tool 暴露给 LLM:`action` 字段
//! 决定走哪条 LSP method,`file_path` 决定路由到哪个 server。
//!
//! ## 范围 (v0.5 Phase A)
//!
//! - transport: 仅 stdio(LSP server 几乎都是 stdio,HTTP 极少)
//! - methods: `textDocument/definition` | `references` | `hover`(只读 MVP)
//! - 配置: `[lsp_servers.<name>]` TOML 表风格
//!
//! ## 不在 Phase A (留 Phase B/C)
//!
//! - `rename` / `codeAction` 落盘
//! - `WorkspaceEdit` 应用
//! - `documentSymbol` / `completion` / `signatureHelp` / `diagnostic`
//! - `publishDiagnostics` notification 缓存
//! - HTTP transport
//! - FileWatcher 增量 `didChange`
//!
//! ## 关键约束
//!
//! - Tool 名称强制单数 `lsp`,**不**做 `lsp__<server>__<method>` 前缀;
//!   LLM 不该选 server,文件路径决定 server 才符合 LSP 直觉。
//! - 配置走 `[lsp_servers.<name>]` TOML 表风格(对齐 `[mcp_servers]`)。
//! - env 注入用 `env_clear()` 隔离,只传 `HOME`/`PATH` + 用户显式 env,
//!   避免泄漏 `OPENAI_API_KEY` 等密钥到 LSP 子进程。
//! - JSON-RPC framing 严格按 LSP spec:`Content-Length: N\r\n\r\n` + 紧跟
//!   N 字节 body;漏一字节 server 直接断连,`framing` 单元测试必须覆盖
//!   `NeedMore` 路径。

#![allow(clippy::module_name_repetitions)] // 多个模块都暴露 *Config/*Error 等命名

pub mod client;
pub mod config;
pub mod diagnostics;
pub mod document;
pub mod error;
pub mod manager;
pub mod matching;
pub mod methods;
pub mod tool;
pub mod transport;

// 公开 API re-export
pub use client::LspClientInner;
pub use config::{CompiledFilePattern, LspServerConfig};
pub use diagnostics::DiagnosticRegistry;
pub use error::LspError;
pub use manager::{LspConnectionManager, LspLifecycleEvent, LspServerHandle};
pub use methods::LspAction;
pub use tool::LspTool;
