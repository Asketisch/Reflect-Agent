//! Agents capability —— 扫描 plugin 提供的 subagent markdown 定义。
//!
//! 对齐 `loadPluginAgents.ts:37-63`。每个 markdown 文件含 YAML frontmatter,
//! 字段:name / description / when-to-use / tools / disallowedTools / model /
//! maxTurns 等。v0 阶段我们只解析 frontmatter 的 `name` / `description`,
//! 其余字段原样保留为 JSON 字符串待 Phase B 注入 `SubAgentSpec` 时再细分。
//!
//! 命令命名:同 commands,`<plugin_name>:<namespace>:<basename>`。

use std::path::{Path, PathBuf};

use crate::errors::{PluginError, Result};
use crate::manifest::AgentSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAgent {
    pub name: String,
    pub file_path: PathBuf,
    pub description: Option<String>,
    /// 整个 frontmatter 块原文 —— Phase B 注入时细分。
    pub frontmatter_raw: Option<String>,
}

pub fn load(spec: &AgentSpec, plugin_root: &Path, plugin_name: &str) -> Result<Vec<LoadedAgent>> {
    let dirs: Vec<PathBuf> = match spec {
        AgentSpec::None => return Ok(Vec::new()),
        AgentSpec::Path(p) => vec![plugin_root.join(p)],
        AgentSpec::Paths(paths) => paths.iter().map(|p| plugin_root.join(p)).collect(),
    };

    let mut out = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            tracing::warn!(path = %dir.display(), "plugin agents 目录不存在");
            continue;
        }
        scan_dir_recursive(&dir, &dir, plugin_name, &mut out)?;
    }
    Ok(out)
}

fn scan_dir_recursive(
    base: &Path,
    current: &Path,
    plugin_name: &str,
    out: &mut Vec<LoadedAgent>,
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
        } else if ty.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let mut parts: Vec<String> = Vec::new();
            if let Some(parent) = rel.parent() {
                for seg in parent.components() {
                    if let std::path::Component::Normal(s) = seg
                        && let Some(s) = s.to_str()
                    {
                        parts.push(s.to_string());
                    }
                }
            }
            let stem = rel
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("agent")
                .to_string();
            let name = if parts.is_empty() {
                format!("{plugin_name}:{stem}")
            } else {
                format!("{plugin_name}:{}:{stem}", parts.join(":"))
            };
            let (description, frontmatter_raw) = parse_agent_metadata(&path)?;
            out.push(LoadedAgent {
                name,
                file_path: path,
                description,
                frontmatter_raw,
            });
        }
    }
    Ok(())
}

/// 提取 frontmatter 的 `description` 与整段 YAML 原文。
/// 与 commands 共用解析模式 —— v0 简化版,失败回退 None。
fn parse_agent_metadata(path: &Path) -> Result<(Option<String>, Option<String>)> {
    let text = std::fs::read_to_string(path).map_err(|e| PluginError::ManifestIo {
        path: path.to_path_buf(),
        source: e,
    })?;
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((None, None));
    }
    let rest = &trimmed[3..];
    let Some(end) = rest.find("\n---") else {
        return Ok((None, None));
    };
    let front = &rest[..end];
    let raw = Some(front.to_string());
    let mut desc = None;
    for line in front.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            desc = Some(v.to_string());
            break;
        }
    }
    Ok((desc, raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_none() {
        let tmp = TempDir::new().unwrap();
        assert!(
            load(&AgentSpec::None, tmp.path(), "demo")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn load_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
        std::fs::write(
            tmp.path().join("agents/review.md"),
            "---\ndescription: Review PR\n---\n# body\n",
        )
        .unwrap();
        let out = load(&AgentSpec::Path("./agents".into()), tmp.path(), "demo").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "demo:review");
        assert_eq!(out[0].description.as_deref(), Some("Review PR"));
        assert!(out[0].frontmatter_raw.is_some());
    }

    #[test]
    fn load_missing_dir_warns() {
        let tmp = TempDir::new().unwrap();
        let out = load(&AgentSpec::Path("./nonexistent".into()), tmp.path(), "demo").unwrap();
        assert!(out.is_empty());
    }
}
