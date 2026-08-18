//! `LoadSkillTool` —— 按名字激活 skill 的 `Tool` 实现。
//!
//! 参数:`{ "name": "<skill 名>" }`。返回含 skill 正文与刚激活工具
//! 列表的 JSON。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput};

use crate::catalog::SkillsCatalog;

/// 注册到 `ToolRegistry` 的工具名。
pub const LOAD_SKILL_NAME: &str = "load_skill";

/// `load_skill(name)` 的 `Tool` 实现。持有 `Arc<SkillsCatalog>`,
/// 让激活结果在下一次迭代的 `pre_loop` 可见。
pub struct LoadSkillTool {
    catalog: Arc<SkillsCatalog>,
}

impl std::fmt::Debug for LoadSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadSkillTool").finish_non_exhaustive()
    }
}

impl LoadSkillTool {
    /// 绑定 catalog 构造新工具。
    pub fn new(catalog: Arc<SkillsCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        LOAD_SKILL_NAME
    }

    fn description(&self) -> &str {
        "Load a skill by name. Returns the skill's body and activates its declared tools."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill name (from the available skills catalog)"
                }
            },
            "required": ["name"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let name =
            args.get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing required `name` argument".into(),
                })?;
        let skill = self
            .catalog
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("skill not found: {name}"),
            })?;
        self.catalog.activate(name);
        let activated_tools = skill.tools.clone();
        let payload = json!({
            "name": skill.name,
            "body": skill.body,
            "activated_tools": activated_tools,
        });
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text {
                text: serde_json::to_string(&payload).unwrap_or_default(),
            }],
            is_error: false,
            metadata: json!({ "skill": name, "activated_tools": activated_tools }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SkillsCatalog;
    use crate::model::SkillMeta;
    use std::path::PathBuf;

    fn make_skill(name: &str, tools: Vec<&str>) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: format!("{name} skill"),
            triggers: vec![],
            tools: tools.into_iter().map(String::from).collect(),
            mcp_collections: vec![],
            path: PathBuf::new(),
            body: format!("# {name}\nbody"),
            plugin_id: None,
            when_paths: vec![],
        }
    }

    #[tokio::test]
    async fn load_skill_returns_body_and_activated_tools() {
        let cat = SkillsCatalog::new();
        cat.insert(make_skill("foo", vec!["read", "grep"]));
        let cat = Arc::new(cat);
        let tool = LoadSkillTool::new(cat.clone());

        let out = tool
            .execute(ToolContext::default(), json!({"name": "foo"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(cat.is_activated("foo"));
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text,
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["name"], "foo");
        assert_eq!(parsed["activated_tools"][0], "read");
        assert_eq!(parsed["activated_tools"][1], "grep");
    }

    #[tokio::test]
    async fn unknown_skill_returns_invalid_args() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        let err = tool
            .execute(ToolContext::default(), json!({"name": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn missing_name_arg_returns_invalid_args() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        let err = tool
            .execute(ToolContext::default(), json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn tool_metadata_is_concurrency_safe() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), LOAD_SKILL_NAME);
    }
}
