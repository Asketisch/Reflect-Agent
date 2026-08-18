//! `discovery` —— 路径条件技能激活(P2 `skill-discovery`)。
//!
//! 当 workspace 相对路径匹配 `SkillMeta::when_paths` 时自动激活技能。

use std::path::Path;

use globset::{Glob, GlobMatcher};

use crate::model::SkillMeta;

/// 编译后的路径条件。
#[derive(Debug, Clone)]
pub struct PathCondition {
    matcher: GlobMatcher,
}

impl PathCondition {
    pub fn compile(glob: &str) -> Result<Self, globset::Error> {
        Ok(Self {
            matcher: Glob::new(glob)?.compile_matcher(),
        })
    }

    pub fn matches(&self, rel_path: &str) -> bool {
        self.matcher.is_match(rel_path)
    }
}

/// 扫描 catalog,返回与 `rel_path` 匹配应激活的技能名。
pub fn skills_for_path(skills: &[SkillMeta], rel_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for s in skills {
        if s.when_paths.is_empty() {
            continue;
        }
        if s.when_paths.iter().any(|g| {
            Glob::new(g)
                .ok()
                .map(|x| x.compile_matcher().is_match(rel_path))
                .unwrap_or(false)
        }) {
            out.push(s.name.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 判断单个技能是否应在给定路径下激活。
pub fn skill_matches_path(skill: &SkillMeta, rel_path: &str) -> bool {
    if skill.when_paths.is_empty() {
        return false;
    }
    skill.when_paths.iter().any(|g| {
        Glob::new(g)
            .ok()
            .map(|x| x.compile_matcher().is_match(rel_path))
            .unwrap_or(false)
    })
}

/// 从绝对路径推导相对 workspace 的路径字符串。
pub fn relativize(workspace: &Path, file: &Path) -> Option<String> {
    file.strip_prefix(workspace)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SkillMeta;
    use std::path::PathBuf;

    fn skill(name: &str, when: Vec<&str>) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: "d".into(),
            triggers: vec![],
            tools: vec![],
            mcp_collections: vec![],
            path: PathBuf::new(),
            body: String::new(),
            plugin_id: None,
            when_paths: when.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn matches_rust_files() {
        let s = skill("rust-review", vec!["**/*.rs"]);
        assert!(skill_matches_path(&s, "src/main.rs"));
        assert!(!skill_matches_path(&s, "README.md"));
    }

    #[test]
    fn skills_for_path_returns_multiple() {
        let skills = vec![skill("a", vec!["**/*.rs"]), skill("b", vec!["**/test/**"])];
        let names = skills_for_path(&skills, "crates/foo/test/x.rs");
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }
}
