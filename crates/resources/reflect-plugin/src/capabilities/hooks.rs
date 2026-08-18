//! Hooks capability —— 解析 plugin 提供的 hook 配置。
//!
//! 来源(`manifest.hooks` 三种形态,`schemas.ts:348-373`):
//! - `None` —— 无 hook
//! - `Path("./hooks/extra.json")` —— 指向 hooks.json 文件
//! - `Inline(json)` —— 内联 JSON
//! - `Many([...])` —— 多源叠加
//!
//! v0 阶段我们只解析 JSON 结构,不真正构造 `reflect_hooks::Hook` 对象
//! (那一步依赖 `Hook` trait 的具体 impl,由 Phase B 接管)。每个 hook
//! 声明给出事件名 + matcher + 实际命令 —— Phase B 把命令包装成
//! `Hook` 实例。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::{PluginError, Result};
use crate::manifest::HookSpec;

/// 解析后的 hook 配置 —— 承载 raw JSON 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHook {
    /// 来源:文件路径(若来自 Path 形态)或 `"<inline>"`。
    pub source: String,
    /// 事件名(如 `PreToolUse` / `PostToolUse` / `Stop`)。
    pub event: String,
    /// matcher 字符串(如 tool 名 glob),`None` 表示无 matcher。
    pub matcher: Option<String>,
    /// hook 命令(数组 / 字符串 / 命令对象)。
    pub command: serde_json::Value,
}

/// hooks.json 顶层结构 —— 简化版,只解析 `{hooks: {EventName: [{matcher, hooks: [...]}]}}`。
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: HooksMap,
}

type HooksMap = std::collections::BTreeMap<String, Vec<HookMatcherEntry>>;

#[derive(Debug, Deserialize)]
struct HookMatcherEntry {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookCommandEntry>,
}

#[derive(Debug, Deserialize)]
struct HookCommandEntry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

pub fn load(spec: &HookSpec, plugin_root: &Path) -> Result<Vec<LoadedHook>> {
    let sources = match spec {
        HookSpec::None => return Ok(Vec::new()),
        HookSpec::Path(p) => vec![SourceOrJson::Path(plugin_root.join(p))],
        HookSpec::Inline(v) => vec![SourceOrJson::Inline(v.clone())],
        HookSpec::Many(items) => items
            .iter()
            .map(|s| match s {
                HookSpec::Path(p) => Ok(SourceOrJson::Path(plugin_root.join(p))),
                HookSpec::Inline(v) => Ok(SourceOrJson::Inline(v.clone())),
                HookSpec::None => Err(PluginError::Validation(
                    "HookSpec::Many 不允许嵌套 None".into(),
                )),
                HookSpec::Many(_) => Err(PluginError::Validation(
                    "HookSpec::Many 不允许嵌套(深度限制 1)".into(),
                )),
            })
            .collect::<Result<Vec<_>>>()?,
    };

    let mut out = Vec::new();
    for source in sources {
        let (label, json) = match source {
            SourceOrJson::Path(p) => {
                let label = p.display().to_string();
                let text = std::fs::read_to_string(&p).map_err(|e| PluginError::ManifestIo {
                    path: p.clone(),
                    source: e,
                })?;
                let json: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| PluginError::ManifestParse {
                        path: p.clone(),
                        message: e.to_string(),
                    })?;
                (label, json)
            }
            SourceOrJson::Inline(v) => ("<inline>".into(), v.clone()),
        };
        expand(&label, &json, &mut out)?;
    }
    Ok(out)
}

enum SourceOrJson {
    Path(PathBuf),
    Inline(serde_json::Value),
}

fn expand(source: &str, json: &serde_json::Value, out: &mut Vec<LoadedHook>) -> Result<()> {
    let parsed: HooksFile =
        serde_json::from_value(json.clone()).map_err(|e| PluginError::ManifestParse {
            path: PathBuf::from(source),
            message: e.to_string(),
        })?;
    for (event, entries) in parsed.hooks {
        for entry in entries {
            for cmd in entry.hooks {
                let cmd_value = serde_json::json!({
                    "type": cmd.kind,
                    "command": cmd.command,
                    "timeout": cmd.timeout,
                });
                out.push(LoadedHook {
                    source: source.to_string(),
                    event: event.clone(),
                    matcher: entry.matcher.clone(),
                    command: cmd_value,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_none() {
        let tmp = TempDir::new().unwrap();
        assert!(load(&HookSpec::None, tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn load_path_reads_hooks_json() {
        let tmp = TempDir::new().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "bash", "hooks": [
                            { "type": "command", "command": "echo blocked" }
                        ] }
                    ]
                }
            }"#,
        )
        .unwrap();
        let out = load(&HookSpec::Path("./hooks.json".into()), tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "PreToolUse");
        assert_eq!(out[0].matcher.as_deref(), Some("bash"));
        assert!(out[0].command["command"] == "echo blocked");
    }

    #[test]
    fn load_inline_parses_json() {
        let json = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "matcher": null, "hooks": [
                        { "type": "command", "command": "cleanup" }
                    ] }
                ]
            }
        });
        let out = load(&HookSpec::Inline(json), Path::new("/")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "Stop");
        assert_eq!(out[0].matcher, None);
    }

    #[test]
    fn load_many_combines_sources() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("a.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"x","hooks":[{"type":"command","command":"a"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":null,"hooks":[{"type":"command","command":"b"}]}]}}"#,
        )
        .unwrap();
        let spec = HookSpec::Many(vec![
            HookSpec::Path("./a.json".into()),
            HookSpec::Path("./b.json".into()),
        ]);
        let out = load(&spec, tmp.path()).unwrap();
        assert_eq!(out.len(), 2);
    }
}
