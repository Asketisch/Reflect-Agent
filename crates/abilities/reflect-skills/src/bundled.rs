//! `bundled` —— 内置技能包(P2 `bundled-skills`)。
//!
//! 编译期嵌入常用技能元数据;启动时可 merge 进 `SkillsCatalog`。

use std::path::PathBuf;

use crate::model::SkillMeta;

/// 内置技能 catalog(15 项:P2 `bundled-skills` 目标达成)。
pub fn bundled_skills() -> Vec<SkillMeta> {
    vec![
        SkillMeta {
            name: "code-review".into(),
            description: "Review code changes for bugs and style".into(),
            triggers: vec!["review".into(), "pr".into()],
            tools: vec!["read".into(), "grep".into()],
            mcp_collections: vec![],
            path: PathBuf::from("bundled/code-review/SKILL.md"),
            body: include_str!("bundled/code-review.md").to_string(),
            plugin_id: None,
            when_paths: vec!["**/*.rs".into(), "**/*.ts".into()],
        },
        SkillMeta {
            name: "commit-helper".into(),
            description: "Draft conventional commit messages".into(),
            triggers: vec!["commit".into()],
            tools: vec!["bash".into(), "read".into()],
            mcp_collections: vec![],
            path: PathBuf::from("bundled/commit-helper/SKILL.md"),
            body: include_str!("bundled/commit-helper.md").to_string(),
            plugin_id: None,
            when_paths: vec![],
        },
        SkillMeta {
            name: "test-runner".into(),
            description: "Run and interpret test output".into(),
            triggers: vec!["test".into(), "cargo test".into()],
            tools: vec!["bash".into()],
            mcp_collections: vec![],
            path: PathBuf::from("bundled/test-runner/SKILL.md"),
            body: include_str!("bundled/test-runner.md").to_string(),
            plugin_id: None,
            when_paths: vec!["**/Cargo.toml".into()],
        },
        // v1.2.0:补齐至 15 项常用编码技能。
        skill(
            "debug-troubleshoot",
            "Systematic debugging: reproduce, locate, hypothesize, verify, fix, review",
            &["debug", "bug", "crash"],
            &["read", "bash"],
        ),
        skill(
            "refactor-extract",
            "Safe refactoring: extract functions/interfaces, eliminate duplication",
            &["refactor", "extract"],
            &["read", "edit"],
        ),
        skill(
            "docstring-gen",
            "Generate doc comments explaining intent, not restating logic",
            &["doc", "docstring", "docs"],
            &["read", "edit"],
        ),
        skill(
            "dependency-audit",
            "Audit deps: outdated, CVEs, licenses, unused crates",
            &["deps", "outdated", "audit"],
            &["bash"],
        ),
        skill(
            "perf-profile",
            "Locate and fix performance bottlenecks (measure before optimizing)",
            &["perf", "profile", "slow", "benchmark"],
            &["bash"],
        ),
        skill(
            "git-conflict-resolve",
            "Resolve merge/rebase conflicts by merging intent, not picking sides",
            &["conflict", "merge", "rebase"],
            &["bash", "read", "edit"],
        ),
        skill(
            "dockerize",
            "Build production container images (multi-stage, minimal, cached layers)",
            &["docker", "container", "image"],
            &["bash", "write"],
        ),
        skill(
            "migrate-database",
            "Incremental, reversible database schema migrations",
            &["migrate", "migration", "schema"],
            &["bash"],
        ),
        skill(
            "i18n-extract",
            "Extract user-visible strings into i18n resources",
            &["i18n", "translate", "localize"],
            &["read", "edit"],
        ),
        skill(
            "security-scan",
            "Audit code for common vulnerability classes (injection, auth, secrets)",
            &["security", "vuln", "cve"],
            &["read", "grep"],
        ),
        skill(
            "api-design",
            "Design clear, evolvable APIs (minimal surface, type-driven, explicit errors)",
            &["api", "trait", "interface"],
            &["read", "edit"],
        ),
        skill(
            "cli-build",
            "Build ergonomic CLIs (discoverable, consistent flags, helpful errors)",
            &["cli", "clap", "argparse"],
            &["read", "edit"],
        ),
    ]
}

/// 构造一个内联 body 的内置技能 helper(减少重复样板)。
fn skill(name: &str, description: &str, triggers: &[&str], tools: &[&str]) -> SkillMeta {
    let body = match name {
        "debug-troubleshoot" => include_str!("bundled/debug-troubleshoot.md"),
        "refactor-extract" => include_str!("bundled/refactor-extract.md"),
        "docstring-gen" => include_str!("bundled/docstring-gen.md"),
        "dependency-audit" => include_str!("bundled/dependency-audit.md"),
        "perf-profile" => include_str!("bundled/perf-profile.md"),
        "git-conflict-resolve" => include_str!("bundled/git-conflict-resolve.md"),
        "dockerize" => include_str!("bundled/dockerize.md"),
        "migrate-database" => include_str!("bundled/migrate-database.md"),
        "i18n-extract" => include_str!("bundled/i18n-extract.md"),
        "security-scan" => include_str!("bundled/security-scan.md"),
        "api-design" => include_str!("bundled/api-design.md"),
        "cli-build" => include_str!("bundled/cli-build.md"),
        _ => "",
    };
    SkillMeta {
        name: name.into(),
        description: description.into(),
        triggers: triggers.iter().map(|s| s.to_string()).collect(),
        tools: tools.iter().map(|s| s.to_string()).collect(),
        mcp_collections: vec![],
        path: PathBuf::from(format!("bundled/{name}/SKILL.md")),
        body: body.to_string(),
        plugin_id: None,
        when_paths: vec![],
    }
}

/// 把 bundled skills 插入 catalog(跳过已存在的同名技能)。
pub fn merge_bundled(catalog: &crate::catalog::SkillsCatalog) -> usize {
    let mut added = 0;
    for s in bundled_skills() {
        if catalog.get(&s.name).is_none() {
            catalog.insert(s);
            added += 1;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_has_at_least_fifteen() {
        assert!(
            bundled_skills().len() >= 15,
            "P2 目标 15+,实际 {}",
            bundled_skills().len()
        );
    }

    #[test]
    fn bundled_skill_names_are_unique() {
        let skills = bundled_skills();
        let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "内置技能名重复");
    }

    #[test]
    fn merge_bundled_is_idempotent() {
        let cat = crate::catalog::SkillsCatalog::new();
        let n1 = merge_bundled(&cat);
        let n2 = merge_bundled(&cat);
        assert!(n1 >= 15);
        assert_eq!(n2, 0);
    }
}
