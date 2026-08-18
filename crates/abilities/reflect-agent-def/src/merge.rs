//! 将 Markdown 解析出的 agent 定义与 TOML 默认值层合并。
//!
//! 镜像 Reflect `agent_definitions.py:merge_definitions`。
//! 凡 Markdown 已设置的字段(非 `None` / 非空 / 非默认)以 Markdown
//! 值为准;TOML 只填补 `None` 槽位。
use toml::Value;

use crate::model::AgentDefinition;

/// 将 Markdown 解析的 `md` 与 TOML 默认值表合并。
/// `toml` 值是 `toml::value::Table`(即 `toml::Value::Table`)。
pub fn merge_with_toml(mut md: AgentDefinition, toml: Option<Value>) -> AgentDefinition {
    let Some(Value::Table(t)) = toml else {
        return md;
    };
    // name:仅当 MD 的为空时用 TOML。
    if md.name.is_empty() {
        if let Some(Value::String(s)) = t.get("name") {
            md.name = s.clone();
        }
    }
    if md.description.is_empty() {
        if let Some(Value::String(s)) = t.get("description") {
            md.description = s.clone();
        }
    }
    if !md.spawnable {
        if let Some(Value::Boolean(b)) = t.get("spawnable") {
            md.spawnable = *b;
        }
    }
    if !md.readonly {
        if let Some(Value::Boolean(b)) = t.get("readonly") {
            md.readonly = *b;
        }
    }
    if md.tools.is_empty() {
        if let Some(arr) = t.get("tools").and_then(|v| v.as_array()) {
            md.tools = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    if md.disallowed_tools.is_empty() {
        if let Some(arr) = t.get("disallowed_tools").and_then(|v| v.as_array()) {
            md.disallowed_tools = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    if md.model.is_none() {
        if let Some(Value::String(s)) = t.get("model") {
            md.model = Some(s.clone());
        }
    }
    if md.max_turns.is_none() {
        if let Some(Value::Integer(n)) = t.get("max_turns") {
            md.max_turns = Some(*n as u32);
        }
    }
    // system_prompt 恒为 MD 独有(TOML 无对应字段)。
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_agent_str;

    const MD: &str = "---\nname: from-md\ndescription: desc\n---\nMD body\n";

    #[test]
    fn merge_with_no_toml_returns_md_unchanged() {
        let md = parse_agent_str(MD).unwrap();
        let merged = merge_with_toml(md.clone(), None);
        assert_eq!(merged.name, "from-md");
        assert_eq!(merged.description, "desc");
        assert_eq!(merged.system_prompt, "MD body");
    }

    #[test]
    fn merge_toml_fills_none_slots() {
        let md = parse_agent_str(MD).unwrap();
        let toml: Value = toml::toml! {
            spawnable = true
            readonly = true
            max_turns = 50
            tools = ["read", "write"]
        }
        .into();
        let merged = merge_with_toml(md, Some(toml));
        // name / description 仍来自 MD。
        assert_eq!(merged.name, "from-md");
        assert_eq!(merged.description, "desc");
        // spawnable、readonly、max_turns、tools 来自 TOML。
        assert!(merged.spawnable);
        assert!(merged.readonly);
        assert_eq!(merged.max_turns, Some(50));
        assert_eq!(merged.tools, vec!["read", "write"]);
        // system_prompt 恒为 MD。
        assert_eq!(merged.system_prompt, "MD body");
    }

    #[test]
    fn merge_md_wins_on_conflict() {
        let md = parse_agent_str(
            "---\nname: x\ndescription: d\nmax_turns: 100\nspawnable: true\n---\nbody",
        )
        .unwrap();
        let toml: Value = toml::toml! {
            max_turns = 10
            spawnable = false
        }
        .into();
        let merged = merge_with_toml(md, Some(toml));
        assert_eq!(merged.max_turns, Some(100));
        assert!(merged.spawnable);
    }

    #[test]
    fn merge_with_non_table_value_ignored() {
        let md = parse_agent_str(MD).unwrap();
        let merged = merge_with_toml(md.clone(), Some(Value::String("hi".into())));
        // 应保持不变。
        assert_eq!(merged.name, md.name);
        assert_eq!(merged.system_prompt, md.system_prompt);
    }
}
