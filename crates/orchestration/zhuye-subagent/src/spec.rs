//! `SubAgentSpec` —— 父级可通过 `call_<role>` 工具 spawn 的子代理声明式描述。

use crate::data_transfer::DataTransferConfig;

/// 一个具名子代理。`role` 决定父级 LLM 用来调用该 agent 的工具名(`call_<role>`)。
#[derive(Debug, Clone)]
pub struct SubAgentSpec {
    /// 人类可读的标签(用于日志)。
    pub name: String,
    /// 短标识符 —— 也是工具名后缀。必须 snake_case,且在父级范围内唯一
    /// (registry 不做检查)。
    pub role: String,
    /// Model spec(例如 `"openai/gpt-4o"`);`None` 时沿用父级 model。
    pub model: Option<String>,
    /// 前置到子 agent 对话开头的 system prompt。
    pub system_prompt: String,
    /// 子 agent 可调用的父级工具名称子集。空 = 无工具(纯文本子代理)。
    pub allowed_tools: Vec<String>,
    /// 父级 context 的传入方式与子 agent 最终回答的抽取方式。
    pub data_transfer: DataTransferConfig,
    /// v1.x:per-subagent `model_call` 迭代上限(取与全局上限的 `min`)。
    /// `None` = 沿用全局 `active.max_iterations` / 默认 32,向后兼容。
    pub max_turns: Option<u32>,
    /// v1.x:per-subagent skill 白名单(非空时硬性过滤 child 可见 skill)。
    /// `Vec::new()` = 继承父全部 skill(向后兼容)。
    pub allowed_skills: Vec<String>,
}

impl SubAgentSpec {
    /// `call_<role>` —— 暴露给父级 LLM 的工具名。
    pub fn tool_name(&self) -> String {
        format!("call_{}", self.role)
    }

    /// 校验 spec。若 `role` 为空或含会干扰 JSON-Schema 参数列表的字符,
    /// 返回 `Err`。
    pub fn validate(&self) -> Result<(), String> {
        if self.role.is_empty() {
            return Err("role cannot be empty".into());
        }
        if !self
            .role
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "role '{}' must be lowercase alphanumeric + _-",
                self.role
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_from_role() {
        let s = SubAgentSpec {
            name: "Explorer".into(),
            role: "explorer".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert_eq!(s.tool_name(), "call_explorer");
    }

    #[test]
    fn validate_accepts_lowercase_role() {
        let s = SubAgentSpec {
            name: "x".into(),
            role: "explorer-v2".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_rejects_uppercase() {
        let s = SubAgentSpec {
            name: "x".into(),
            role: "Explorer".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_role() {
        let s = SubAgentSpec {
            name: "x".into(),
            role: "".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert!(s.validate().is_err());
    }
}
