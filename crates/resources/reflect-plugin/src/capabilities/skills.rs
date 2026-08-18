//! Skills capability —— 扫描 plugin 提供的 SKILL.md。
//!
//! 目录约定:
//! - `skills/<name>/SKILL.md` —— 子目录形式,`<name>` 作 skill 名
//! - `skills/<name>/SKILL.md` 的所有非 SKILL.md 文件是 skill 的附件,
//!   此 loader 不读取它们,只记录 SKILL.md 路径

use std::path::{Path, PathBuf};

use crate::errors::{PluginError, Result};
use crate::manifest::SkillSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    /// skill 名 —— `<name>` 段。Phase B 注入 `SkillsCatalog` 时与
    /// plugin 命名空间组合。
    pub name: String,
    /// SKILL.md 文件绝对路径。
    pub skill_md: PathBuf,
    pub description: Option<String>,
}

pub fn load(spec: &SkillSpec, plugin_root: &Path) -> Result<Vec<LoadedSkill>> {
    let dirs: Vec<PathBuf> = match spec {
        SkillSpec::None => return Ok(Vec::new()),
        SkillSpec::Path(p) => vec![plugin_root.join(p)],
        SkillSpec::Paths(paths) => paths.iter().map(|p| plugin_root.join(p)).collect(),
    };

    let mut out = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            tracing::warn!(path = %dir.display(), "plugin skills 目录不存在");
            continue;
        }
        scan_dir(&dir, &mut out)?;
    }
    Ok(out)
}

fn scan_dir(dir: &Path, out: &mut Vec<LoadedSkill>) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| PluginError::ManifestIo {
        path: dir.to_path_buf(),
        source: e,
    })? {
        let entry = entry.map_err(|e| PluginError::ManifestIo {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| PluginError::ManifestIo {
            path: path.clone(),
            source: e,
        })?;
        if !ty.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            // 跳过没有 SKILL.md 的目录。
            continue;
        }
        let description = extract_skill_description(&skill_md).ok().flatten();
        out.push(LoadedSkill {
            name,
            skill_md,
            description,
        });
    }
    Ok(())
}

fn extract_skill_description(path: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| PluginError::ManifestIo {
        path: path.to_path_buf(),
        source: e,
    })?;
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return Ok(None);
    }
    let rest = &trimmed[3..];
    let Some(end) = rest.find("\n---") else {
        return Ok(None);
    };
    let front = &rest[..end];
    for line in front.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            return Ok(Some(v.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_skill(root: &Path, name: &str, desc: &str) {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: \"{desc}\"\n---\n# body\n"),
        )
        .unwrap();
    }

    #[test]
    fn load_none() {
        let tmp = TempDir::new().unwrap();
        assert!(load(&SkillSpec::None, tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn load_path() {
        let tmp = TempDir::new().unwrap();
        make_skill(tmp.path(), "lint", "Lint skill");
        make_skill(tmp.path(), "format", "Format skill");
        let out = load(&SkillSpec::Path("./skills".into()), tmp.path()).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|s| s.name == "lint"));
        assert!(out.iter().any(|s| s.name == "format"));
        assert!(
            out.iter()
                .find(|s| s.name == "lint")
                .unwrap()
                .description
                .as_deref()
                .is_some()
        );
    }

    #[test]
    fn load_skips_dirs_without_skill_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("skills/with")).unwrap();
        std::fs::write(
            tmp.path().join("skills/with/SKILL.md"),
            "---\ndescription: x\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("skills/without")).unwrap();
        // 无 SKILL.md。
        let out = load(&SkillSpec::Path("./skills".into()), tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "with");
    }
}
