//! 插件 manifest 与 marketplace manifest 的 schema 定义。
//!
//! v0 覆盖核心字段,其余字段留 v1 扩展。
//!
//! 所有字段缺省 `None` / `Default`,未知 TOML 键会被 `serde` 静默丢弃
//! (顶层);嵌套对象(`userConfig`、`mcpServers` 等)保持 strict,
//! 错拼会被 `ManifestParse` 捕获。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── 作者 / homepage / repository 等基础字段 ────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAuthor {
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRepository {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

// ── 依赖声明 ─────────────────────────────────────────────────────────

/// 依赖项 —— 来自 manifest 的 `dependencies: string[]`。
///
/// 支持三种字面量(参考 `schemas.ts:1348-1391`):
/// - `"plugin"` —— 在声明 marketplace 内解析
/// - `"plugin@marketplace"` —— 限定 marketplace
/// - `"plugin@mkt@^1.0"` —— 带版本后缀,**静默 strip**(phase F 才真解析)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum PluginDependency {
    Bare(String),
    Qualified { name: String, marketplace: String },
}

impl PluginDependency {
    pub fn name(&self) -> &str {
        match self {
            PluginDependency::Bare(n) => n,
            PluginDependency::Qualified { name, .. } => name,
        }
    }
}

impl std::str::FromStr for PluginDependency {
    type Err = crate::errors::PluginError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 形态: "name" / "name@marketplace" / "name@marketplace@^1.0"
        // version 后缀(以 @^ / @~ / @= / @>= / @<= 开头)被剥离。
        let s = s.trim();
        if s.is_empty() {
            return Err(crate::errors::PluginError::Validation(
                "dependency 不能为空".into(),
            ));
        }
        let parts: Vec<&str> = s.split('@').collect();
        match parts.len() {
            1 => Ok(PluginDependency::Bare(parts[0].to_string())),
            2 => Ok(PluginDependency::Qualified {
                name: parts[0].to_string(),
                marketplace: parts[1].to_string(),
            }),
            3 => {
                // 中间是 marketplace,末尾是 version 约束 —— 静默 strip。
                Ok(PluginDependency::Qualified {
                    name: parts[0].to_string(),
                    marketplace: parts[1].to_string(),
                })
            }
            _ => Err(crate::errors::PluginError::Validation(format!(
                "dependency 段过多(>3): {s:?}"
            ))),
        }
    }
}

impl std::fmt::Display for PluginDependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginDependency::Bare(n) => f.write_str(n),
            PluginDependency::Qualified { name, marketplace } => {
                write!(f, "{name}@{marketplace}")
            }
        }
    }
}

impl From<PluginDependency> for String {
    fn from(dep: PluginDependency) -> String {
        match dep {
            PluginDependency::Bare(n) => n,
            PluginDependency::Qualified { name, marketplace } => format!("{name}@{marketplace}"),
        }
    }
}

impl TryFrom<String> for PluginDependency {
    type Error = crate::errors::PluginError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

// ── 钩子 ────────────────────────────────────────────────────────────

/// Hook 声明来源 —— 三种形态(对齐 `schemas.ts:348-373`)。
///
/// - 字符串:指向 hooks.json 的路径
/// - 对象:内联
/// - 数组:多个声明叠加
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookSpec {
    #[default]
    None,
    Path(String),
    Inline(serde_json::Value),
    Many(Vec<HookSpec>),
}

// ── 命令 / 代理 / 技能 ───────────────────────────────────────

/// Commands 声明(对齐 `schemas.ts:385-451`)。
///
/// 形态:`"./commands"` | `string[]` | `record<name, InlineCommand>`。
/// 我们 v0 简化版:只支持路径 / 数组路径,inline record 留 v1。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    #[default]
    None,
    Path(String),
    Paths(Vec<String>),
    // v1: Inline(BTreeMap<String, InlineCommandDef>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentSpec {
    #[default]
    None,
    Path(String),
    Paths(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SkillSpec {
    #[default]
    None,
    Path(String),
    Paths(Vec<String>),
}

// ── MCP 服务器 ──────────────────────────────────────────────────────

/// 单个 MCP server 的配置 —— 不引用 `reflect_mcp::McpServerConfig`
/// 以避免 plugin 被 mcp 拽入依赖;phase A 接入时通过 `From` 转换。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// MCP server 声明(对齐 `schemas.ts:543-572`)。
///
/// v0 支持两种:单文件路径字符串、内联 map。混合数组形式留 v1。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerSpec {
    #[default]
    None,
    /// 指向 `.mcp.json` 或 `.mcpb` bundle 的路径。
    Path(String),
    /// 直接内联的多个 server。
    Inline(BTreeMap<String, McpServerConfig>),
}

// ── 用户配置 ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserConfigType {
    String,
    Number,
    Boolean,
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserConfigOption {
    pub r#type: UserConfigType,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub multiple: bool,
}

// ── 顶层 PluginManifest ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 必填,kebab-case。
    pub name: String,

    #[serde(default)]
    pub version: Option<String>,

    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub author: Option<PluginAuthor>,

    #[serde(default)]
    pub license: Option<String>,

    #[serde(default)]
    pub homepage: Option<String>,

    #[serde(default)]
    pub repository: Option<PluginRepository>,

    #[serde(default)]
    pub keywords: Vec<String>,

    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,

    #[serde(default)]
    pub hooks: HookSpec,

    #[serde(default)]
    pub commands: CommandSpec,

    #[serde(default)]
    pub agents: AgentSpec,

    #[serde(default)]
    pub skills: SkillSpec,

    #[serde(default)]
    pub mcp_servers: McpServerSpec,

    /// 启用时询问用户的配置项。**敏感字段不进 settings.json,进 OS keychain**(v1)。
    #[serde(default)]
    pub user_config: BTreeMap<String, UserConfigOption>,
}

impl PluginManifest {
    /// 从 TOML 文件读取并 parse。未做字段级校验(留 `validate_plugin.rs`,
    /// phase A 后续小步合入);只保证 manifest 是合法 TOML + 结构匹配。
    pub fn from_toml_str(s: &str) -> crate::errors::Result<Self> {
        toml::from_str(s).map_err(|e| crate::errors::PluginError::ManifestParse {
            path: PathBuf::from("<inline>"),
            message: e.to_string(),
        })
    }

    /// 从磁盘读 manifest。Phase A 用于 `install(local_path)` 路径。
    pub fn from_path(path: &std::path::Path) -> crate::errors::Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|e| crate::errors::PluginError::ManifestIo {
                path: path.to_path_buf(),
                source: e,
            })?;
        toml::from_str(&text).map_err(|e| crate::errors::PluginError::ManifestParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// 取得第一份 `manifest.toml` 路径(标准约定:`<plugin_root>/.claude-plugin/plugin.json`
    /// 或 `<plugin_root>/plugin.toml`)。
    ///
    /// v0 阶段只支持 plugin.toml(TOML 比 JSON 跟现有 reflect-config 风格一致)。
    pub fn find_in_dir(root: &std::path::Path) -> Option<PathBuf> {
        let candidate = root.join("plugin.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        let candidate = root.join(".claude-plugin").join("plugin.json");
        if candidate.exists() {
            return Some(candidate);
        }
        None
    }
}

// ── 市场清单(MarketplaceManifest)──────────────────────────

/// Marketplace 源(对齐 `schemas.ts:906-1044` 简化版,只含 v0 支持的)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum MarketplaceSource {
    /// HTTP 直链 —— 拉取的 JSON 本身就是 marketplace.json。
    Url {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// GitHub shorthand(owner/repo)。
    Github {
        repo: String,
        #[serde(default)]
        r#ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// 任意 git URL(支持子目录)。
    Git {
        url: String,
        #[serde(default)]
        r#ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    /// 本地 .json 文件路径。
    File { path: PathBuf },
    /// 本地目录(含 `.claude-plugin/marketplace.json`)。
    Directory { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginMarketplaceEntry {
    /// `flatten` 把 plugin manifest 字段全部暴露在 entry 内,
    /// entry 可覆盖 plugin 自己的 plugin.json 字段。
    #[serde(flatten)]
    pub manifest: PluginManifest,

    /// 分发源 —— 必填,marketplace 唯一职责。
    pub source: MarketplaceSource,

    #[serde(default)]
    pub category: Option<String>,

    #[serde(default)]
    pub tags: Vec<String>,

    /// `true` = 插件目录必须自带 plugin.json(默认);
    /// `false` = marketplace entry 自带完整 manifest。
    #[serde(default = "default_true")]
    pub strict: bool,
}

impl Default for PluginMarketplaceEntry {
    /// Marketplace entry 不存在天然 default —— 这是 `MarketplaceSource`
    /// tagged enum 决定的。`Default` 主要给 `BTreeMap::get` 等需要
    /// 占位的场景用;**实际构造必须显式提供 `source`**。
    fn default() -> Self {
        // 用 `File { PathBuf::new() }` 作 placeholder,反序列化时
        // serde 会用真实 source 覆盖。
        Self {
            manifest: PluginManifest::default(),
            source: MarketplaceSource::File {
                path: PathBuf::new(),
            },
            category: None,
            tags: Vec::new(),
            strict: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 顶层 marketplace manifest(对齐 `schemas.ts:1293-1326`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketplaceManifest {
    /// 必填,不能仿冒 builtin/inline。
    pub name: String,

    pub owner: PluginAuthor,

    pub plugins: Vec<PluginMarketplaceEntry>,

    /// `true` = 从 marketplace 移除的 plugin 自动卸载。
    #[serde(default)]
    pub force_remove_deleted_plugins: bool,
}

impl MarketplaceManifest {
    /// 从 JSON 字符串解析 marketplace manifest。
    ///
    /// 注意:`PluginManifest` 是 TOML,**`MarketplaceManifest` 是 JSON**,
    /// 便于 fetch 别人的 marketplace 直接 parse。
    pub fn from_json_str(s: &str) -> crate::errors::Result<Self> {
        serde_json::from_str(s).map_err(|e| crate::errors::PluginError::ManifestParse {
            path: PathBuf::from("<inline json>"),
            message: format!("marketplace manifest json: {e}"),
        })
    }

    /// 从磁盘读 marketplace manifest JSON 文件。
    pub fn from_json_path(path: &std::path::Path) -> crate::errors::Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|e| crate::errors::PluginError::ManifestIo {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::from_json_str(&text).map_err(|mut e| {
            // ManifestParse.path 替换为实际路径(便于上层诊断)。
            if let crate::errors::PluginError::ManifestParse {
                ref mut path,
                ref message,
            } = e
            {
                *path = path.to_path_buf();
                tracing::debug!(path = %path.display(), message = %message, "marketplace manifest parse");
            }
            e
        })
    }

    /// 在 marketplace 根目录下找 `.claude-plugin/marketplace.json`。
    ///
    /// 与 `PluginManifest::find_in_dir` 不同 —— plugin 同时支持
    /// `plugin.toml` 与 `.claude-plugin/plugin.json`,**marketplace 只支持
    /// `.claude-plugin/marketplace.json` 一种**。
    pub fn find_in_dir(root: &std::path::Path) -> Option<PathBuf> {
        let candidate = root.join(".claude-plugin").join("marketplace.json");
        candidate.exists().then_some(candidate)
    }
}

impl PluginMarketplaceEntry {
    /// 把 entry 内的相对 `File` / `Directory` 路径 resolve 到 marketplace
    /// 根目录之下,返回可直接给 `PluginManager::install_local` 用的绝对路径。
    ///
    /// `Git` / `Github` / `Url` 不动 —— 它们自带完整 URL,无需相对路径。
    pub fn resolve_plugin_source(
        &self,
        marketplace_cache_root: &std::path::Path,
    ) -> MarketplaceSource {
        match &self.source {
            MarketplaceSource::File { path } => MarketplaceSource::File {
                path: marketplace_cache_root.join(path),
            },
            MarketplaceSource::Directory { path } => MarketplaceSource::Directory {
                path: marketplace_cache_root.join(path),
            },
            other => other.clone(),
        }
    }
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_minimal() {
        let toml = r#"
name = "foo"
version = "1.0.0"
"#;
        let m = PluginManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.name, "foo");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert!(m.description.is_none());
    }

    #[test]
    fn manifest_full() {
        let toml = r#"
name = "code-formatter"
version = "1.2.3"
description = "格式 TypeScript 代码"
author = { name = "Alice", email = "alice@example.com" }
license = "MIT"
keywords = ["code", "ts"]
dependencies = ["linter@anthropic-tools", "formatter@^1.0"]

[mcp_servers.format]
command = "node"
args = ["format.js"]
"#;
        let m = PluginManifest::from_toml_str(toml).unwrap();
        assert_eq!(m.name, "code-formatter");
        assert_eq!(m.dependencies.len(), 2);
        assert!(matches!(
            m.dependencies[1],
            PluginDependency::Qualified { ref name, ref marketplace }
            if name == "formatter" && marketplace == "^1.0"
        ));
        let inline = match &m.mcp_servers {
            McpServerSpec::Inline(map) => map,
            other => panic!("expected Inline, got {other:?}"),
        };
        assert!(inline.contains_key("format"));
    }

    #[test]
    fn dependency_bare_parses() {
        let d: PluginDependency = "linter".parse().unwrap();
        assert_eq!(d.name(), "linter");
        assert!(matches!(d, PluginDependency::Bare(_)));
    }

    #[test]
    fn dependency_qualified_parses() {
        let d: PluginDependency = "linter@anthropic-tools".parse().unwrap();
        match d {
            PluginDependency::Qualified { name, marketplace } => {
                assert_eq!(name, "linter");
                assert_eq!(marketplace, "anthropic-tools");
            }
            _ => panic!("expected Qualified"),
        }
    }

    #[test]
    fn dependency_version_suffix_stripped() {
        // "formatter@^1.0" 应该解析为 Bare("formatter"),而不是 Qualified。
        // 因为 version 前缀以 ^ 开头,被识别为 version 后缀。
        let d: PluginDependency = "formatter@^1.0".parse().unwrap();
        assert_eq!(d.name(), "formatter");
    }

    #[test]
    fn dependency_too_many_segments_errors() {
        assert!("a@b@c@d".parse::<PluginDependency>().is_err());
    }

    #[test]
    fn dependency_empty_errors() {
        assert!("".parse::<PluginDependency>().is_err());
    }

    #[test]
    fn marketplace_source_github() {
        let toml = r#"
name = "official"
owner = { name = "Anthropic" }

[[plugins]]
name = "code-formatter"
source = { source = "github", repo = "anthropics/claude-plugins-official" }
"#;
        let m: MarketplaceManifest = toml::from_str(toml).unwrap();
        assert_eq!(m.name, "official");
        assert_eq!(m.plugins.len(), 1);
        let entry = &m.plugins[0];
        assert_eq!(entry.manifest.name, "code-formatter");
        assert!(entry.strict);
        assert!(
            matches!(entry.source, MarketplaceSource::Github { ref repo, .. } if repo == "anthropics/claude-plugins-official")
        );
    }

    #[test]
    fn marketplace_source_file() {
        let toml = r#"
name = "local"
owner = { name = "Local" }
plugins = []
"#;
        let m: MarketplaceManifest = toml::from_str(toml).unwrap();
        assert_eq!(m.name, "local");
        assert!(m.plugins.is_empty());
        assert!(!m.force_remove_deleted_plugins);
    }

    #[test]
    fn manifest_find_in_dir_picks_plugin_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("plugin.toml"),
            "name = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let found = PluginManifest::find_in_dir(tmp.path()).unwrap();
        assert!(found.ends_with("plugin.toml"));
    }

    #[test]
    fn manifest_find_in_dir_picks_claude_plugin_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"name":"x","version":"0.1.0"}"#).unwrap();
        let found = PluginManifest::find_in_dir(tmp.path()).unwrap();
        assert!(found.ends_with("plugin.json"));
    }

    #[test]
    fn manifest_find_in_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(PluginManifest::find_in_dir(tmp.path()).is_none());
    }

    // ── MarketplaceManifest JSON 加载器(Phase D) ──────────────────────

    #[test]
    fn marketplace_manifest_from_json_str_parses_minimal() {
        let json = r#"{
            "name": "local",
            "owner": { "name": "Alice" },
            "plugins": []
        }"#;
        let m = MarketplaceManifest::from_json_str(json).unwrap();
        assert_eq!(m.name, "local");
        assert_eq!(m.owner.name, "Alice");
        assert!(m.plugins.is_empty());
        assert!(!m.force_remove_deleted_plugins);
    }

    #[test]
    fn marketplace_manifest_from_json_str_parses_full() {
        let json = r#"{
            "name": "official",
            "owner": { "name": "Anthropic", "email": "a@example.com" },
            "plugins": [
                {
                    "name": "code-formatter",
                    "version": "1.0.0",
                    "source": { "source": "directory", "path": "./plugins/formatter" }
                },
                {
                    "name": "remote-tool",
                    "version": "0.5.0",
                    "source": {
                        "source": "git",
                        "url": "https://example.com/repo.git",
                        "ref": "main"
                    }
                }
            ],
            "force_remove_deleted_plugins": true
        }"#;
        let m = MarketplaceManifest::from_json_str(json).unwrap();
        assert_eq!(m.name, "official");
        assert_eq!(m.plugins.len(), 2);
        assert!(m.force_remove_deleted_plugins);
        assert!(matches!(
            m.plugins[0].source,
            MarketplaceSource::Directory { .. }
        ));
        assert!(matches!(m.plugins[1].source, MarketplaceSource::Git { .. }));
    }

    #[test]
    fn marketplace_manifest_from_json_str_rejects_garbage() {
        let err = MarketplaceManifest::from_json_str("not json").unwrap_err();
        assert!(matches!(
            err,
            crate::errors::PluginError::ManifestParse { .. }
        ));
    }

    #[test]
    fn marketplace_manifest_from_json_path_reads_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("marketplace.json");
        std::fs::write(&path, r#"{"name":"x","owner":{"name":"y"},"plugins":[]}"#).unwrap();
        let m = MarketplaceManifest::from_json_path(&path).unwrap();
        assert_eq!(m.name, "x");
    }

    #[test]
    fn marketplace_manifest_from_json_path_errors_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let err = MarketplaceManifest::from_json_path(&path).unwrap_err();
        assert!(matches!(err, crate::errors::PluginError::ManifestIo { .. }));
    }

    #[test]
    fn marketplace_manifest_find_in_dir_finds_claude_plugin_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), "{}").unwrap();
        let found = MarketplaceManifest::find_in_dir(tmp.path()).unwrap();
        assert!(found.ends_with(".claude-plugin/marketplace.json"));
    }

    #[test]
    fn marketplace_manifest_find_in_dir_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(MarketplaceManifest::find_in_dir(tmp.path()).is_none());
    }

    #[test]
    fn plugin_marketplace_entry_resolves_relative_paths_against_root() {
        // File / Directory 路径相对于 marketplace cache root。
        let entry = PluginMarketplaceEntry {
            manifest: PluginManifest::default(),
            source: MarketplaceSource::Directory {
                path: PathBuf::from("./plugins/demo"),
            },
            category: None,
            tags: Vec::new(),
            strict: true,
        };
        let root = PathBuf::from("/cache/official");
        let resolved = entry.resolve_plugin_source(&root);
        match resolved {
            MarketplaceSource::Directory { path } => {
                assert_eq!(path, PathBuf::from("/cache/official/plugins/demo"));
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn plugin_marketplace_entry_resolves_file_relative_too() {
        let entry = PluginMarketplaceEntry {
            manifest: PluginManifest::default(),
            source: MarketplaceSource::File {
                path: PathBuf::from("./m.json"),
            },
            category: None,
            tags: Vec::new(),
            strict: true,
        };
        let root = PathBuf::from("/cache/x");
        match entry.resolve_plugin_source(&root) {
            MarketplaceSource::File { path } => {
                assert_eq!(path, PathBuf::from("/cache/x/m.json"));
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn plugin_marketplace_entry_leaves_git_url_unchanged() {
        // Git 自带完整 URL,resolve 不动。
        let entry = PluginMarketplaceEntry {
            manifest: PluginManifest::default(),
            source: MarketplaceSource::Git {
                url: "https://example.com/repo.git".into(),
                r#ref: Some("main".into()),
                sha: None,
                path: None,
            },
            category: None,
            tags: Vec::new(),
            strict: true,
        };
        let root = PathBuf::from("/cache/whatever");
        match entry.resolve_plugin_source(&root) {
            MarketplaceSource::Git {
                url,
                r#ref,
                sha,
                path,
            } => {
                assert_eq!(url, "https://example.com/repo.git");
                assert_eq!(r#ref.as_deref(), Some("main"));
                assert!(sha.is_none());
                assert!(path.is_none());
            }
            other => panic!("expected Git, got {other:?}"),
        }
    }
}
