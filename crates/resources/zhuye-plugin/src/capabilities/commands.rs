//! Commands capability —— 扫描 plugin 提供的 slash command markdown。
//!
//! 来源(`manifest.commands` 三种形态):
//! - `None` —— 不挂任何 command
//! - `Path("./commands")` —— 单目录
//! - `Paths(["a/commands", "b/commands"])` —— 多目录
//!
//! 目录约定:
//! - `*.md` 文件被识别为 command 文件
//! - 文件名(去 `.md`)是命令的 basename
//! - 子目录路径作为 namespace,最终命令名 `<plugin_name>:<namespace>:<basename>`
//! - 递归扫到第一层(不递归进 SKILL.md)

use std::path::{Path, PathBuf};

use crate::errors::{PluginError, Result};
use crate::manifest::CommandSpec;

/// 加载后的 command 一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCommand {
    /// 命令名 —— `<plugin_id.name()>:<rel_path_segments>` 形式,
    /// 例如 plugin `foo`, 文件 `commands/utils/lint.md` →
    /// `foo:utils:lint`。命令命名约定。
    pub name: String,
    /// markdown 文件绝对路径(在 plugin install_path 下)。
    pub file_path: PathBuf,
    /// 命令说明(从 markdown frontmatter 的 `description` 字段读,
    /// v0 阶段只解析最简形态)。
    pub description: Option<String>,
}

/// 加载 commands。`spec` 描述来源,`plugin_root` 是 plugin 安装目录。
pub fn load(
    spec: &CommandSpec,
    plugin_root: &Path,
    plugin_name: &str,
) -> Result<Vec<LoadedCommand>> {
    let dirs: Vec<PathBuf> = match spec {
        CommandSpec::None => return Ok(Vec::new()),
        CommandSpec::Path(p) => vec![resolve_relative(plugin_root, p)?],
        CommandSpec::Paths(paths) => paths
            .iter()
            .map(|p| resolve_relative(plugin_root, p))
            .collect::<Result<Vec<_>>>()?,
    };

    let mut out = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            // 路径声明但不存在 —— warn 但不阻断(作者可能预留)。
            tracing::warn!(path = %dir.display(), "plugin commands 目录不存在");
            continue;
        }
        scan_dir_recursive(&dir, &dir, plugin_name, &mut out)?;
    }
    Ok(out)
}

fn resolve_relative(plugin_root: &Path, p: &str) -> Result<PathBuf> {
    let path = plugin_root.join(p);
    // path traversal 防护:仅在两侧都能 canonicalize 时校验。macOS
    // `/var/folders` 是 `/private/var/folders` 的 symlink,不存在的路径
    // canonicalize 会失败 —— 此时跳过校验,后续 `exists()` 检查会兜住。
    if let (Ok(canonical_root), Ok(canonical_path)) =
        (plugin_root.canonicalize(), path.canonicalize())
        && !canonical_path.starts_with(&canonical_root)
    {
        return Err(PluginError::Validation(format!(
            "command 路径逃出 plugin 目录: {p}"
        )));
    }
    Ok(path)
}

fn scan_dir_recursive(
    base: &Path,
    current: &Path,
    plugin_name: &str,
    out: &mut Vec<LoadedCommand>,
) -> Result<()> {
    for entry in std::fs::read_dir(current).map_err(|e| PluginError::ManifestIo {
        path: current.to_path_buf(),
        source: e,
    })? {
        let entry = entry.map_err(|e| PluginError::ManifestIo {
            path: current.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| PluginError::ManifestIo {
            path: path.clone(),
            source: e,
        })?;
        if ty.is_dir() {
            scan_dir_recursive(base, &path, plugin_name, out)?;
        } else if ty.is_file() {
            let ext = path.extension().and_then(|s| s.to_str());
            if ext != Some("md") {
                continue;
            }
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let name = build_command_name(plugin_name, rel);
            let description = extract_description(&path).ok().flatten();
            out.push(LoadedCommand {
                name,
                file_path: path,
                description,
            });
        }
    }
    Ok(())
}

fn build_command_name(plugin_name: &str, rel_path: &Path) -> String {
    // 路径段拼成 namespace,文件名(去 .md)作为 basename。
    let mut parts: Vec<String> = Vec::new();
    if let Some(parent) = rel_path.parent() {
        for seg in parent.components() {
            if let std::path::Component::Normal(s) = seg
                && let Some(s) = s.to_str()
            {
                parts.push(s.to_string());
            }
        }
    }
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmd")
        .to_string();
    if parts.is_empty() {
        format!("{plugin_name}:{stem}")
    } else {
        format!("{plugin_name}:{}:{stem}", parts.join(":"))
    }
}

/// 从 markdown frontmatter 提取 `description` 字段。v0 简化版:只解析
/// YAML frontmatter(用 `---` 包裹)的最顶层 `description` 键。
/// 不依赖 `serde_yaml`,自己手撕以避免引入大依赖。
fn extract_description(path: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| PluginError::ManifestIo {
        path: path.to_path_buf(),
        source: e,
    })?;
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return Ok(None);
    }
    // 找第二个 `---` 收尾。
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

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn load_none_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let out = load(&CommandSpec::None, tmp.path(), "demo").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_path_scans_md_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_file(
            root,
            "commands/hello.md",
            "---\ndescription: greet\n---\n# hi\n",
        );
        make_file(root, "commands/sub/lint.md", "# lint\n");
        let out = load(&CommandSpec::Path("./commands".into()), root, "demo").unwrap();
        assert_eq!(out.len(), 2);
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"demo:hello"));
        assert!(names.contains(&"demo:sub:lint"));
    }

    #[test]
    fn load_skips_non_md_files() {
        let tmp = TempDir::new().unwrap();
        make_file(tmp.path(), "commands/hi.md", "# hi\n");
        make_file(tmp.path(), "commands/notes.txt", "ignore me");
        let out = load(&CommandSpec::Path("./commands".into()), tmp.path(), "demo").unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn load_missing_dir_warns_but_succeeds() {
        let tmp = TempDir::new().unwrap();
        let out = load(
            &CommandSpec::Path("./nonexistent".into()),
            tmp.path(),
            "demo",
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_multiple_paths() {
        let tmp = TempDir::new().unwrap();
        make_file(tmp.path(), "cmds/a.md", "a");
        make_file(tmp.path(), "more/b.md", "b");
        let out = load(
            &CommandSpec::Paths(vec!["./cmds".into(), "./more".into()]),
            tmp.path(),
            "demo",
        )
        .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn extract_description_parses_yaml_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        std::fs::write(&p, "---\ndescription: \"Hello world\"\n---\n# body\n").unwrap();
        let desc = extract_description(&p).unwrap();
        assert_eq!(desc.as_deref(), Some("Hello world"));
    }

    #[test]
    fn extract_description_returns_none_for_plain_md() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        std::fs::write(&p, "# no frontmatter\n").unwrap();
        let desc = extract_description(&p).unwrap();
        assert_eq!(desc, None);
    }

    #[test]
    fn build_command_name_handles_top_level_and_nested() {
        assert_eq!(
            build_command_name("foo", Path::new("hello.md")),
            "foo:hello"
        );
        assert_eq!(
            build_command_name("foo", Path::new("utils/lint.md")),
            "foo:utils:lint"
        );
        assert_eq!(
            build_command_name("foo", Path::new("a/b/c/deep.md")),
            "foo:a:b:c:deep"
        );
    }
}
