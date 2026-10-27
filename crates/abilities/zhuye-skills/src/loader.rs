//! SKILL.md 文件的 frontmatter 解析器。
//!
//! 对齐旧版 `skill_loader.py:parse_skill_frontmatter` 的行为。

use std::fs;
use std::path::Path;

use crate::model::{SkillError, SkillMeta};

/// 从字符串解析 SKILL.md。
pub fn parse_skill_str(input: &str) -> Result<SkillMeta, SkillError> {
    let (front, body) = split_frontmatter(input).ok_or(SkillError::NoFrontmatter)?;
    let mut meta: SkillMeta = if front.trim().is_empty() {
        return Err(SkillError::NoFrontmatter);
    } else {
        serde_yaml::from_str(front)?
    };
    meta.body = body.trim().to_string();
    if meta.name.is_empty() {
        return Err(SkillError::MissingField("name"));
    }
    if meta.description.is_empty() {
        return Err(SkillError::MissingField("description"));
    }
    Ok(meta)
}

/// 从文件路径解析 SKILL.md。
pub fn parse_skill_file(path: &Path) -> Result<SkillMeta, SkillError> {
    let raw = fs::read_to_string(path)?;
    let mut meta = parse_skill_str(&raw)?;
    meta.path = path.to_path_buf();
    Ok(meta)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nname: test\ndescription: d\n---\nbody\n";

    #[test]
    fn parse_minimal() {
        let m = parse_skill_str(MINIMAL).unwrap();
        assert_eq!(m.name, "test");
        assert_eq!(m.body, "body");
    }

    #[test]
    fn parse_with_tools() {
        let input =
            "---\nname: x\ndescription: d\ntools: [read, grep]\ntriggers: [review]\n---\nbody\n";
        let m = parse_skill_str(input).unwrap();
        assert_eq!(m.tools, vec!["read", "grep"]);
        assert_eq!(m.triggers, vec!["review"]);
    }

    #[test]
    fn missing_name_errors() {
        let input = "---\ndescription: d\n---\nbody";
        let err = parse_skill_str(input).unwrap_err();
        assert!(matches!(err, SkillError::MissingField("name")));
    }

    #[test]
    fn missing_description_errors() {
        let input = "---\nname: x\n---\nbody";
        let err = parse_skill_str(input).unwrap_err();
        assert!(matches!(err, SkillError::MissingField("description")));
    }

    #[test]
    fn no_frontmatter_errors() {
        let err = parse_skill_str("just text").unwrap_err();
        assert!(matches!(err, SkillError::NoFrontmatter));
    }
}
