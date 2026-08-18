//! v1.x 功能 2:Markdown frontmatter subagent spec 加载器。
//!
//! 扫描 `~/.reflect/subagents/*.md` 与 `<workspace>/.reflect/subagents/*.md`,
//! 解析 YAML frontmatter + Markdown body 为
//! [`reflect_config::SubagentSpecConfig`]。frontmatter 字段与 `[[subagents]]`
//! TOML 段完全一致;Markdown body 作为 `system_prompt`(若 frontmatter 未显式
//! 给 `system_prompt`,body 覆盖空值)。
//!
//! 优先级(由 `reflect-exec::bootstrap_m5` 合并):
//! **TOML `[[subagents]]` > workspace Markdown > home Markdown > 硬编码 explorer**。
//! 同 `role` 冲突时高优先级覆盖低优先级。
//!
//! 复用 `reflect-agent-def` 的 frontmatter 切分模式(`---` 分隔的 YAML 块)。

use std::fs;
use std::path::Path;

use reflect_config::SubagentSpecConfig;

/// 解析单个 Markdown 字符串为 `SubagentSpecConfig`。frontmatter 是文件顶部
/// `---` 行界定的 YAML 块;其后 body 作为 `system_prompt`。
///
/// 若 frontmatter 未提供 `system_prompt`(空字符串),则用 Markdown body
/// 填充 —— 让用户既能用 frontmatter 短描述,也能用 body 长提示词。
pub fn parse_subagent_str(input: &str) -> Result<SubagentSpecConfig, LoadError> {
    let (front, body) = split_frontmatter(input).ok_or(LoadError::NoFrontmatter)?;
    let mut cfg: SubagentSpecConfig = if front.trim().is_empty() {
        return Err(LoadError::EmptyFrontmatter);
    } else {
        serde_yaml::from_str(front)?
    };
    // body 作为 system_prompt 兜底(frontmatter 显式值优先)。
    let body_trimmed = body.trim();
    if cfg.system_prompt.is_empty() && !body_trimmed.is_empty() {
        cfg.system_prompt = body_trimmed.to_string();
    }
    validate(&cfg)?;
    Ok(cfg)
}

/// 解析单个 `.md` 文件。
pub fn parse_subagent_md(path: &Path) -> Result<SubagentSpecConfig, LoadError> {
    let raw = fs::read_to_string(path)?;
    parse_subagent_str(&raw)
}

/// 遍历目录,解析每个 `*.md` 为 `SubagentSpecConfig`,以 `role` 为 key 返回。
/// 解析失败的文件经 `tracing::warn!` 记录后跳过(不中断整体加载)。
/// 目录不存在时返回空 map。
pub fn load_subagents_dir(dir: &Path) -> Result<Vec<SubagentSpecConfig>, LoadError> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match parse_subagent_md(&path) {
            Ok(cfg) => out.push(cfg),
            Err(e) => {
                tracing::warn!(?path, ?e, "failed to load subagent definition; skipping");
            }
        }
    }
    Ok(out)
}

/// 合并多源 subagent spec,按优先级去重(后出现的覆盖先出现的同 `role`)。
///
/// 入参顺序应为「低 → 高」优先级(例如 `[home_md, workspace_md, toml]`),
/// 高优先级的 `role` 覆盖低优先级。
pub fn merge_by_priority(sources: Vec<Vec<SubagentSpecConfig>>) -> Vec<SubagentSpecConfig> {
    use std::collections::HashMap;
    let mut by_role: HashMap<String, SubagentSpecConfig> = HashMap::new();
    // 保持首次出现顺序,后续同 role 仅覆盖值。
    let mut order: Vec<String> = Vec::new();
    for src in sources {
        for cfg in src {
            let role = cfg.role.clone();
            if !by_role.contains_key(&role) {
                order.push(role.clone());
            }
            by_role.insert(role, cfg);
        }
    }
    order
        .into_iter()
        .filter_map(|r| by_role.remove(&r))
        .collect()
}

/// 校验:`role` 非空且 snake_case(与 `SubAgentSpec::validate` 一致)。
fn validate(cfg: &SubagentSpecConfig) -> Result<(), LoadError> {
    if cfg.role.is_empty() {
        return Err(LoadError::MissingField("role"));
    }
    if !cfg
        .role
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(LoadError::InvalidRole(cfg.role.clone()));
    }
    if cfg.name.is_empty() {
        return Err(LoadError::MissingField("name"));
    }
    Ok(())
}

/// 把输入拆分为 `(frontmatter, body)`,与 `reflect_agent_def::parser::split_frontmatter` 风格一致。
fn split_frontmatter(input: &str) -> Option<(&str, &str)> {
    let after_open = input.strip_prefix("---")?;
    let mut rest_start: Option<usize> = None;
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        if line.trim_start().starts_with("---") {
            rest_start = Some(line_start);
            break;
        }
    }
    let line_start = rest_start?;
    let front = &after_open[..line_start];
    let body = &after_open[offset..];
    Some((front, body))
}

/// Loader 错误。
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("missing frontmatter (expected `---\\n...\\n---\\n`)")]
    NoFrontmatter,
    #[error("empty frontmatter")]
    EmptyFrontmatter,
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid role '{0}' (must be lowercase alphanumeric + _-)")]
    InvalidRole(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nname: Researcher\nrole: researcher\n---\nYou research things.\n";

    #[test]
    fn parse_minimal_frontmatter() {
        let cfg = parse_subagent_str(MINIMAL).unwrap();
        assert_eq!(cfg.name, "Researcher");
        assert_eq!(cfg.role, "researcher");
        assert_eq!(cfg.system_prompt, "You research things.");
    }

    #[test]
    fn parse_all_optional_fields() {
        let input = r#"---
name: Coder
role: coder
model: anthropic/claude-3-5-sonnet-latest
system_prompt: "explicit prompt"
allowed_tools: [read, write, bash]
allowed_skills: [code-review]
max_turns: 15
---
body ignored when system_prompt set
"#;
        let cfg = parse_subagent_str(input).unwrap();
        assert_eq!(cfg.name, "Coder");
        assert_eq!(
            cfg.model.as_deref(),
            Some("anthropic/claude-3-5-sonnet-latest")
        );
        assert_eq!(cfg.system_prompt, "explicit prompt");
        assert_eq!(cfg.allowed_tools, vec!["read", "write", "bash"]);
        assert_eq!(cfg.allowed_skills, vec!["code-review"]);
        assert_eq!(cfg.max_turns, Some(15));
    }

    #[test]
    fn body_fills_empty_system_prompt() {
        let input = "---\nname: X\nrole: x\n---\n# Body\n\nlong prompt here\n";
        let cfg = parse_subagent_str(input).unwrap();
        assert!(cfg.system_prompt.contains("long prompt here"));
    }

    #[test]
    fn missing_role_errors() {
        let input = "---\nname: x\n---\nbody";
        let err = parse_subagent_str(input).unwrap_err();
        assert!(matches!(err, LoadError::MissingField("role")));
    }

    #[test]
    fn uppercase_role_errors() {
        let input = "---\nname: x\nrole: BadRole\n---\nbody";
        let err = parse_subagent_str(input).unwrap_err();
        assert!(matches!(err, LoadError::InvalidRole(_)));
    }

    #[test]
    fn no_frontmatter_errors() {
        let err = parse_subagent_str("just body").unwrap_err();
        assert!(matches!(err, LoadError::NoFrontmatter));
    }

    #[test]
    fn load_dir_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let out = load_subagents_dir(&dir.path().join("nope")).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_dir_parses_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.md"),
            "---\nname: A\nrole: alpha\n---\nbody a\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("skip.txt"), "not md").unwrap();
        let out = load_subagents_dir(dir.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "alpha");
    }

    #[test]
    fn merge_by_priority_high_overrides_low() {
        let low = vec![SubagentSpecConfig {
            name: "A".into(),
            role: "alpha".into(),
            system_prompt: "low".into(),
            ..Default::default()
        }];
        let high = vec![SubagentSpecConfig {
            name: "A2".into(),
            role: "alpha".into(),
            system_prompt: "high".into(),
            ..Default::default()
        }];
        let merged = merge_by_priority(vec![low, high]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].system_prompt, "high");
    }

    #[test]
    fn merge_preserves_distinct_roles_in_order() {
        let a = vec![SubagentSpecConfig {
            name: "A".into(),
            role: "alpha".into(),
            ..Default::default()
        }];
        let b = vec![SubagentSpecConfig {
            name: "B".into(),
            role: "beta".into(),
            ..Default::default()
        }];
        let merged = merge_by_priority(vec![a, b]);
        assert_eq!(merged.len(), 2);
        // 首次出现顺序保留:alpha 先于 beta。
        assert_eq!(merged[0].role, "alpha");
        assert_eq!(merged[1].role, "beta");
    }
}
