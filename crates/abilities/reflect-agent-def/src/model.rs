//! `AgentDefinition` 结构体与错误类型。

use serde::{Deserialize, Serialize};
use thiserror::Error;

use reflect_memory::MemoryScope;

/// `model` 的哨兵值,表示「继承调用方 / `AgentConfig`」。
pub const MODEL_INHERIT: &str = "inherit";

/// 未指定时的默认 agent 名。
pub const DEFAULT_AGENT_NAME: &str = "default";

/// 一条 agent 定义(解析自单个 `agent.md`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// 必填。稳定标识符。
    #[serde(default)]
    pub name: String,
    /// 必填。人类可读的描述。
    #[serde(default)]
    pub description: String,
    /// 其它 agent 是否可以 spawn 本 agent。
    #[serde(default)]
    pub spawnable: bool,
    /// 标记本 agent 为只读(不提供 `write` / `edit` 工具)。
    #[serde(default)]
    pub readonly: bool,
    /// agent 可用的工具名白名单。空 = 全部内置工具。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 工具名显式拒绝列表。
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// `Some("inherit")` → 用调用方提供的 model。`Some(其它)` →
    /// 覆盖。`None` → 用调用方提供的。
    #[serde(default)]
    pub model: Option<String>,
    /// 每回合迭代次数硬上限。
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// 需要加载并注入的 memory scope。
    #[serde(default)]
    pub memory: Vec<MemoryScope>,
    /// 为 v1 MCP 支持预留。v0 忽略。
    #[serde(default)]
    pub mcp_collections: Vec<String>,
    /// Markdown 正文(即 system prompt)。取自文件中 frontmatter
    /// 闭合 `---` 之后的部分。
    #[serde(default)]
    pub system_prompt: String,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: DEFAULT_AGENT_NAME.to_string(),
            description: String::new(),
            spawnable: false,
            readonly: false,
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            max_turns: None,
            memory: Vec::new(),
            mcp_collections: Vec::new(),
            system_prompt: String::new(),
        }
    }
}

/// agent 定义相关错误。
#[derive(Debug, Error)]
pub enum AgentDefError {
    /// I/O 错误。
    #[error("agent def io error: {0}")]
    Io(#[from] std::io::Error),
    /// YAML 解析错误。
    #[error("agent def yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// 缺少必填字段。
    #[error("agent def missing required field: {0}")]
    MissingField(&'static str),
    /// 找不到 frontmatter。
    #[error("agent def missing frontmatter (expected `---\\n...\\n---\\n`)")]
    NoFrontmatter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_documented_defaults() {
        let d = AgentDefinition::default();
        assert_eq!(d.name, DEFAULT_AGENT_NAME);
        assert!(!d.spawnable);
        assert!(!d.readonly);
        assert!(d.tools.is_empty());
        assert!(d.model.is_none());
        assert!(d.system_prompt.is_empty());
    }
}
