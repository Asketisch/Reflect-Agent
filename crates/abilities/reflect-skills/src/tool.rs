//! `LoadSkillTool` —— 按名字激活 skill 的 `Tool` 实现。
//!
//! 参数:`{ "name": "<skill 名>" }`。返回含 skill 正文与刚激活工具
//! 列表的 JSON。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use reflect_tools::{Tool, ToolContext, ToolError, ToolOutput};

use crate::catalog::SkillsCatalog;

/// 注册到 `ToolRegistry` 的工具名。
pub const LOAD_SKILL_NAME: &str = "load_skill";

/// `load_skill(name)` 的 `Tool` 实现。持有 `Arc<SkillsCatalog>`,
/// 让激活结果在下一次迭代的 `pre_loop` 可见。
pub struct LoadSkillTool {
    catalog: Arc<SkillsCatalog>,
}

impl std::fmt::Debug for LoadSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadSkillTool").finish_non_exhaustive()
    }
}

impl LoadSkillTool {
    /// 绑定 catalog 构造新工具。
    pub fn new(catalog: Arc<SkillsCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        LOAD_SKILL_NAME
    }

    fn description(&self) -> &str {
        "Load a skill by name. Returns the skill's body and activates its declared tools."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill name (from the available skills catalog)"
                }
            },
            "required": ["name"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let name =
            args.get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing required `name` argument".into(),
                })?;
        let skill = self
            .catalog
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("skill not found: {name}"),
            })?;
        self.catalog.activate(name);
        let activated_tools = skill.tools.clone();
        let payload = json!({
            "name": skill.name,
            "body": skill.body,
            "activated_tools": activated_tools,
        });
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text {
                text: serde_json::to_string(&payload).unwrap_or_default(),
            }],
            is_error: false,
            metadata: json!({ "skill": name, "activated_tools": activated_tools }),
            elapsed_ms: 0,
        })
    }
}

// ── v1.4 D3:技能附属资源读取(渐进披露第三级) ─────────────────────

/// `read_skill_resource` 工具名。
pub const READ_SKILL_RESOURCE_NAME: &str = "read_skill_resource";

/// 单个附属资源的读取上限。
const RESOURCE_MAX_BYTES: u64 = 64 * 1024;

/// `read_skill_resource(skill, path)` —— 读取 skill 目录下的附属文件
/// (scripts / references 等),实现渐进披露的第三级:目录注入只给
/// name + description,`load_skill` 按需拉正文,大体积的附加资料由
/// 本工具按需读取。
///
/// 安全边界:`path` 必须是 skill 目录内的相对路径(拒绝绝对路径与
/// `..` 分量,canonicalize 后校验仍在目录内);单文件上限 64 KiB;
/// `path` 为空或 `.` 时列目录条目(不下发文件内容)。
pub struct ReadSkillResourceTool {
    catalog: Arc<SkillsCatalog>,
}

impl std::fmt::Debug for ReadSkillResourceTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadSkillResourceTool")
            .finish_non_exhaustive()
    }
}

impl ReadSkillResourceTool {
    pub fn new(catalog: Arc<SkillsCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for ReadSkillResourceTool {
    fn name(&self) -> &str {
        READ_SKILL_RESOURCE_NAME
    }

    fn description(&self) -> &str {
        "Read a supporting file (script/reference) that ships with a skill, \
         or list the skill's directory when `path` is empty."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string", "description": "The skill name (from the catalog)"},
                "path": {"type": "string", "description": "Path relative to the skill directory; empty or \".\" lists entries"}
            },
            "required": ["skill"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let name =
            args.get("skill")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing required `skill` argument".into(),
                })?;
        let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let skill = self
            .catalog
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("skill not found: {name}"),
            })?;

        // skill.path 指向 SKILL.md;资源根 = 其父目录。path 为空(插件
        // 提供的内联 skill)时报错。
        let skill_dir = skill
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("skill '{name}' has no on-disk directory to read from"),
            })?;

        if rel.is_empty() || rel == "." {
            // 列目录。
            let mut entries: Vec<String> = Vec::new();
            let mut rd = std::fs::read_dir(skill_dir)
                .map_err(|e| ToolError::Execution(format!("read_dir failed: {e}")))?;
            while let Some(ent) = rd
                .next()
                .transpose()
                .map_err(|e| ToolError::Execution(format!("readdir failed: {e}")))?
            {
                let kind = if ent.path().is_dir() { "dir" } else { "file" };
                entries.push(format!("{} ({})", ent.file_name().to_string_lossy(), kind));
            }
            entries.sort();
            let text = if entries.is_empty() {
                format!("(empty skill directory: {})", skill_dir.display())
            } else {
                format!("entries:\n{}", entries.join("\n"))
            };
            return Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::Text { text }],
                is_error: false,
                metadata: json!({ "skill": name, "mode": "list" }),
                elapsed_ms: 0,
            });
        }

        // 路径安全:组件级拒绝(绝对路径 / `..` / 非法组件),再
        // canonicalize 校验仍在 skill 目录内(防 symlink 逃逸)。
        let rel_path = std::path::Path::new(rel);
        if rel_path.is_absolute() {
            return Err(ToolError::InvalidArgs {
                message: "path must be relative to the skill directory".into(),
            });
        }
        for comp in rel_path.components() {
            match comp {
                std::path::Component::Normal(_) => {}
                _ => {
                    return Err(ToolError::InvalidArgs {
                        message: format!("illegal path component in `{rel}`"),
                    });
                }
            }
        }
        let candidate = skill_dir.join(rel_path);
        let canonical = candidate
            .canonicalize()
            .map_err(|e| ToolError::InvalidArgs {
                message: format!("resource not found: {rel} ({e})"),
            })?;
        let canon_dir = skill_dir
            .canonicalize()
            .unwrap_or_else(|_| skill_dir.to_path_buf());
        if !canonical.starts_with(&canon_dir) {
            return Err(ToolError::InvalidArgs {
                message: "path escapes the skill directory".into(),
            });
        }
        if canonical.is_dir() {
            return Err(ToolError::InvalidArgs {
                message: format!("`{rel}` is a directory; pass a file path or use `.` to list"),
            });
        }

        // 大小上限 + 读取。
        let meta = std::fs::metadata(&canonical)
            .map_err(|e| ToolError::Execution(format!("stat failed: {e}")))?;
        if meta.len() > RESOURCE_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "resource too large: {} bytes (limit {RESOURCE_MAX_BYTES})",
                    meta.len()
                ),
            });
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|e| ToolError::Execution(format!("read failed: {e}")))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text {
                text: format!("[{rel}]\n{text}"),
            }],
            is_error: false,
            metadata: json!({
                "skill": name,
                "path": rel,
                "bytes": meta.len(),
                "version": skill.version,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SkillsCatalog;
    use crate::model::SkillMeta;
    use std::path::PathBuf;

    fn make_skill(name: &str, tools: Vec<&str>) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: format!("{name} skill"),
            triggers: vec![],
            tools: tools.into_iter().map(String::from).collect(),
            mcp_collections: vec![],
            path: PathBuf::new(),
            body: format!("# {name}\nbody"),
            plugin_id: None,
            when_paths: vec![],
            version: None,
        }
    }

    #[tokio::test]
    async fn load_skill_returns_body_and_activated_tools() {
        let cat = SkillsCatalog::new();
        cat.insert(make_skill("foo", vec!["read", "grep"]));
        let cat = Arc::new(cat);
        let tool = LoadSkillTool::new(cat.clone());

        let out = tool
            .execute(ToolContext::default(), json!({"name": "foo"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(cat.is_activated("foo"));
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text,
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["name"], "foo");
        assert_eq!(parsed["activated_tools"][0], "read");
        assert_eq!(parsed["activated_tools"][1], "grep");
    }

    #[tokio::test]
    async fn unknown_skill_returns_invalid_args() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        let err = tool
            .execute(ToolContext::default(), json!({"name": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn missing_name_arg_returns_invalid_args() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        let err = tool
            .execute(ToolContext::default(), json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn tool_metadata_is_concurrency_safe() {
        let cat = Arc::new(SkillsCatalog::new());
        let tool = LoadSkillTool::new(cat);
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.name(), LOAD_SKILL_NAME);
    }
    // ── v1.4 D3:附属资源读取 ────────────────────────────────────

    fn skill_on_disk(dir: &std::path::Path, version: Option<&str>) -> SkillMeta {
        SkillMeta {
            name: "demo".into(),
            description: "demo skill".into(),
            triggers: vec![],
            tools: vec![],
            mcp_collections: vec![],
            path: dir.join("SKILL.md"),
            body: "body".into(),
            version: version.map(String::from),
            plugin_id: None,
            when_paths: vec![],
        }
    }

    #[tokio::test]
    async fn read_resource_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# demo").unwrap();
        std::fs::write(dir.path().join("reference.md"), "lookup table").unwrap();
        let cat = Arc::new(SkillsCatalog::new());
        cat.insert(skill_on_disk(dir.path(), Some("1.2.0")));
        let tool = ReadSkillResourceTool::new(cat);

        let out = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "demo", "path": "reference.md"}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("lookup table"), "got: {text}");
        assert_eq!(out.metadata["version"], "1.2.0");
    }

    #[tokio::test]
    async fn read_resource_list_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# demo").unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        let cat = Arc::new(SkillsCatalog::new());
        cat.insert(skill_on_disk(dir.path(), None));
        let tool = ReadSkillResourceTool::new(cat);

        let out = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "demo", "path": "."}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("a.txt (file)"), "got: {text}");
        assert!(text.contains("scripts (dir)"));
    }

    #[tokio::test]
    async fn read_resource_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# demo").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "s").unwrap();
        let cat = Arc::new(SkillsCatalog::new());
        cat.insert(skill_on_disk(dir.path(), None));
        let tool = ReadSkillResourceTool::new(cat);

        // 相对路径 .. 逃逸。
        let err = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "demo", "path": "../secret.txt"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
        // 绝对路径。
        let err = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "demo", "path": "/etc/passwd"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn read_resource_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "# demo").unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![b'x'; 65 * 1024]).unwrap();
        let cat = Arc::new(SkillsCatalog::new());
        cat.insert(skill_on_disk(dir.path(), None));
        let tool = ReadSkillResourceTool::new(cat);

        let err = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "demo", "path": "big.bin"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { ref message } if message.contains("too large")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn read_resource_inline_skill_no_dir() {
        // 无磁盘路径(plugin 内联 / 测试)→ 明确报错而非 panic。
        let cat = Arc::new(SkillsCatalog::new());
        cat.insert(make_skill("inline", vec![]));
        let tool = ReadSkillResourceTool::new(cat);
        let err = tool
            .execute(
                ToolContext::default(),
                json!({"skill": "inline", "path": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
