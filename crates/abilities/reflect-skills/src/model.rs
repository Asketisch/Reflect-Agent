//! `SkillMeta` 与错误类型。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 一份解析后的 SKILL.md。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// skill 标识。必填。
    #[serde(default)]
    pub name: String,
    /// 触发条件(人类可读)。必填。
    #[serde(default)]
    pub description: String,
    /// 供 LLM 匹配的关键词(如 `["review", "pr"]`)。
    #[serde(default)]
    pub triggers: Vec<String>,
    /// 本 skill 激活的工具。非空时 catalog 会给该 skill 标注
    /// `[+tools: ...]`,`LoadSkillTool::execute` 把这些名字加入
    /// 激活集合。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 为 v1 MCP 支持预留。
    #[serde(default)]
    pub mcp_collections: Vec<String>,
    /// skill 的加载来源文件路径(仅信息性)。
    #[serde(skip)]
    pub path: PathBuf,
    /// Markdown 正文(system-prompt / 工作流)。
    #[serde(default)]
    pub body: String,
    /// v1.4 D3:skill 版本号(frontmatter `version:` 字段,可选)。
    /// 仅信息性:catalog 渲染不携带,资源读取结果可附带。
    #[serde(default)]
    pub version: Option<String>,
    /// v1.0.0-rc2: 如果该 skill 由 plugin 提供,记 plugin id 以便
    /// `remove_plugin_skills(plugin_id)` 一次性反注册。
    /// `None` = 内置或全局 skill(默认)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// P2:路径 glob 条件 —— workspace 相对路径匹配时自动激活。
    #[serde(default, rename = "when")]
    pub when_paths: Vec<String>,
}

/// skill 相关错误。
#[derive(Debug, Error)]
pub enum SkillError {
    /// I/O 错误。
    #[error("skill io error: {0}")]
    Io(#[from] std::io::Error),
    /// YAML 解析错误。
    #[error("skill yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// 缺少必填字段。
    #[error("skill missing required field: {0}")]
    MissingField(&'static str),
    /// 缺少 frontmatter。
    #[error("skill missing frontmatter")]
    NoFrontmatter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_meta_default_traits() {
        let m = SkillMeta {
            name: "x".into(),
            description: "d".into(),
            triggers: vec![],
            tools: vec![],
            mcp_collections: vec![],
            path: PathBuf::from("/tmp/x"),
            body: "body".into(),
            plugin_id: None,
            when_paths: vec![],
            version: None,
        };
        assert_eq!(m.name, "x");
    }
}
