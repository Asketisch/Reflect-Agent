#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! reflect-agent-def —— Markdown + frontmatter 的 agent 定义解析器。
//!
//! Reflect 路线图的 M4。移植自 Reflect `agent_definitions.py`。
//! agent 定义是一个 Markdown 文件:
//!
//! ```markdown
//! ---
//! name: code-reviewer            # agent 名(唯一标识)
//! description: Reviews code changes   # 描述(供选择 agent 时参考)
//! spawnable: false               # 是否可被 spawn 为子代理
//! tools: [read, grep]            # 允许的工具白名单
//! disallowed_tools: [bash, edit] # 显式禁用的工具黑名单
//! model: inherit                 # 模型(inherit = 沿用父级默认)
//! max_turns: 30                  # 最大轮次上限
//! memory: [project, user]        # 记忆作用域
//! mcp_collections: []            # 暴露的 MCP 集合
//! ---
//! # Code Reviewer System Prompt
//!
//! You are a strict code reviewer...
//! ```
//!
//! frontmatter 按 YAML 解析;正文即 `system_prompt`。
//! 可经 [`merge::merge_with_toml`] 在其下叠加 TOML 默认值层。

pub mod merge;
pub mod model;
pub mod parser;

pub use merge::merge_with_toml;
pub use model::{AgentDefError, AgentDefinition, DEFAULT_AGENT_NAME, MODEL_INHERIT};
pub use parser::{load_agents_dir, parse_agent_md, parse_agent_str};
