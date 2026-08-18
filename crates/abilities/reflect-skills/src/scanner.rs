//! `scan_skills_dirs` —— 递归遍历目录寻找 `SKILL.md` 文件。
//!
//! v0 递归遍历每个根目录(优先 `walkdir`,回退手工 BFS)。
//! 每找到一个 `SKILL.md` 就调用 [`parse_skill_file`]。
//! 解析失败的文件记日志后跳过。

use std::fs;
use std::path::Path;

use crate::loader::parse_skill_file;
use crate::model::SkillMeta;

/// 递归遍历每个目录并解析所有 `SKILL.md` 文件。
/// 解析失败的文件记日志后跳过。
pub fn scan_skills_dirs(dirs: &[&Path]) -> Vec<SkillMeta> {
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let walker = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(?dir, ?e, "failed to read skills dir; skipping");
                continue;
            }
        };
        let mut stack: Vec<std::path::PathBuf> = walker.flatten().map(|e| e.path()).collect();
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                if let Ok(entries) = fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        stack.push(entry.path());
                    }
                }
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if path.file_name().and_then(|s| s.to_str()) != Some("SKILL.md") {
                continue;
            }
            match parse_skill_file(&path) {
                Ok(skill) => out.push(skill),
                Err(e) => {
                    tracing::warn!(?path, ?e, "failed to parse SKILL.md; skipping");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let skills = scan_skills_dirs(&[dir.path()]);
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_picks_up_skill_md_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(
            a.join("SKILL.md"),
            "---\nname: a\ndescription: a skill\n---\nbody a\n",
        )
        .unwrap();
        fs::write(
            b.join("SKILL.md"),
            "---\nname: b\ndescription: b skill\n---\nbody b\n",
        )
        .unwrap();
        // 同时创建一个非 `SKILL.md` 文件,确认它会被跳过。
        fs::write(a.join("README.md"), "not a skill").unwrap();

        let skills = scan_skills_dirs(&[dir.path()]);
        assert_eq!(skills.len(), 2);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn scan_skips_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let skills = scan_skills_dirs(&[&missing]);
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_skips_invalid_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "no frontmatter here").unwrap();
        let skills = scan_skills_dirs(&[dir.path()]);
        assert!(skills.is_empty());
    }
}
