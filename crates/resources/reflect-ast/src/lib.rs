//! `reflect-ast` — 基于 tree-sitter 的结构化代码搜索 / 重写工具 (v1.0.0-rc1)。
//!
//! 设计上镜像 `reflect-lsp`:把 tree-sitter 的 5 个默认 grammar(rust /
//! typescript / python / go / javascript)+ feature-gated 的 15+ 扩展
//! grammar 统一暴露为单一 `ast` tool。`action` 字段决定走哪条路径,LLM
//! 不直接接触 grammar 选择 —— `LanguageGrid::for_ext(path)` 按文件后缀
//! 自动路由(对齐 LSP 的 `matching::pick_server_for`)。
//!
//! ## 范围 (v1.0.0-rc1 Phase P0)
//!
//! - `action = list_languages` —— 列出当前 build 启用的所有 grammar,
//!   5 个默认 + feature 启用的扩展。**P0 唯一 live action**。
//! - 其余 action (`search` / `replace` / `rename_symbol`) 留 P1 / P2 / P3
//!   增量补全,P0 调用返回 "not implemented" 错误信息。
//!
//! ## 不在 P0 (留 P1+)
//!
//! - `search` action + 三种 prefix pattern (`kind:<node>` / `regex:<re>` /
//!   `text:<lit>`)
//! - `replace` / `rename_symbol` action + `ToolContext::approval` 重构
//! - Bare-pattern DSL (`fn $NAME(...)`) —— 留 v1+ 单独 PR
//! - 跨文件 rename —— 需 LSP `textDocument/references` 配合
//!
//! ## 关键约束
//!
//! - Tool 名称强制单数 `ast`(对齐 `lsp`),LLM 调 `ast({"action": "..."})`。
//! - grammar 选择按 Cargo feature 编译期决定;`cargo build -p reflect-ast
//!   --features lang-all` 可启用全部 20 个 grammar。runtime 不下载 / 不
//!   spawn 子进程(parser 内存常驻)。
//! - P0 阶段所有 action 走 `Auto`(避免 LLM 早期试探就触发 approval flow);
//!   P3 在 `ToolContext::approval` refactor 完成后切 per-action 表。

#![allow(clippy::module_name_repetitions)] // 多模块暴露 *Config/*Error 等命名

pub mod action;
pub mod edit;
pub mod grid;
pub mod matcher;
pub mod tool;

pub use action::AstAction;
pub use edit::{apply_rename, apply_replace};
pub use grid::{AstError, LanguageGrid, LanguageId, LanguageSpec};
pub use matcher::{CompiledPattern, Hit, search_in_tree, search_kind, search_regex, search_text};
pub use tool::AstTool;
