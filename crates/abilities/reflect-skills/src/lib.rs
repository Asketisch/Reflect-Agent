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
//! reflect-skills —— SKILL.md 扫描、加载与工具激活。
//!
//! Reflect 路线图的 M4。移植自 Reflect `skill_loader.py`。
//!
//! SKILL.md 是带 YAML frontmatter 的 Markdown 文件:
//!
//! ```markdown
//! ---
//! name: code-review              # skill 名(唯一标识)
//! description: Reviews code changes  # 描述(catalog 渲染给 LLM 看)
//! tools: [read, grep]            # 激活后暴露的工具
//! triggers: [review, pr]         # 触发关键词
//! ---
//! # Code Review
//!
//! You are a code reviewer...
//! ```
//!
//! 启动时 [`SkillsCatalog::scan`] 遍历配置目录并解析所有 `SKILL.md`。
//! catalog 渲染一份文本索引,由 `pre_loop` 以 system-reminder 注入。
//! LLM 调用 [`LoadSkillTool`] 激活 skill —— 这会标记该 skill 的
//! `tools:` 列表为激活态,让 [`SkillsCatalog::active_tool_names`]
//! 过滤 LLM 可见的工具 schema。
//!
//! "always-on" 工具名(无需显式激活即对 LLM 可见的工具)由调用方在
//! [`SkillsCatalog::new`] 设置。v0 默认 = `{bash, read, write, edit,
//! grep, glob}`。

pub mod bundled;
pub mod catalog;
pub mod discovery;
pub mod loader;
pub mod model;
pub mod scanner;
pub mod tool;

pub use bundled::{bundled_skills, merge_bundled};
pub use catalog::{ALWAYS_ON_TOOLS, SkillsCatalog, render_catalog};
pub use discovery::{relativize, skill_matches_path, skills_for_path};
pub use loader::{parse_skill_file, parse_skill_str};
pub use model::{SkillError, SkillMeta};
pub use scanner::scan_skills_dirs;
pub use tool::{LoadSkillTool, ReadSkillResourceTool};
