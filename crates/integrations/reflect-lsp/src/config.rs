//! LSP server 强类型配置。
//!
//! 在 `reflect_config::LspServerConfigShape` 之上加一层 "lsp-types-ready" 表示,
//! `LspConnectionManager::start_server` 直接消费本层类型。
//!
//! 与 MCP 不同:LSP 的 `file_patterns` 在本层就完成 glob 预编译
//! (`CompiledFilePattern.glob: globset::GlobSet`),`matching::pick_server_for`
//! 调用时直接 `is_match()` 即可,不需要每次重编译。

use std::collections::HashMap;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use reflect_config::LspServerConfigShape;
use url::Url;

use crate::error::LspError;

/// 编译期就绪的 glob pattern,持有预编译 `GlobSet` + 对应 LSP `languageId`。
///
/// LSP `languageId` 与 LSP 协议里 `TextDocumentItem.languageId` 字符串一致
/// (例如 `"rust"` / `"typescript"` / `"go"`),`textDocument/didOpen` 时
/// 必须填这个值,server 才知道如何语法分析。
#[derive(Debug, Clone)]
pub struct CompiledFilePattern {
    /// 预编译 glob 集合,匹配相对 workspace 的文件路径。
    pub glob: GlobSet,
    /// LSP `languageId`,传给 `didOpen` notification 的
    /// `TextDocumentItem.language_id` 字段。
    pub language_id: String,
}

impl CompiledFilePattern {
    /// 编译单个 pattern。glob 解析失败立即报错,不让 startup 时 panic。
    pub fn compile(glob_str: &str, language_id: impl Into<String>) -> Result<Self, LspError> {
        let glob = Glob::new(glob_str)?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let set = builder.build()?;
        Ok(Self {
            glob: set,
            language_id: language_id.into(),
        })
    }
}

/// 编译期就绪的 LSP server 配置(`reflect_config::LspServerConfigShape` 的薄包装)。
///
/// `reflect-config` 不直接出 `LspServerConfig` 是为了避免它在 `reflect-lsp`
/// 之下依赖过深;这里在 reflect-lsp 侧做形态转换。
#[derive(Debug, Clone)]
pub struct LspServerConfig {
    /// server 名(用户在 `[lsp_servers.<name>]` 里指定的 key)。
    pub name: String,
    /// stdio 子进程可执行文件路径(LSP 全部走 stdio)。
    pub command: String,
    /// stdio 子进程参数。
    pub args: Vec<String>,
    /// 除 `HOME`/`PATH` 之外注入到子进程的 env(避免泄漏父进程 API key)。
    pub env: HashMap<String, String>,
    /// 文件 glob → language_id 映射(预编译)。决定 server 接管哪些文件。
    pub patterns: Vec<CompiledFilePattern>,
    /// 覆盖 LSP server 的 `rootUri`;`None` 走 ctx.workspace 转 file:// URI。
    pub root_uri: Option<Url>,
    /// 透传给 `initialize` 请求的 `initializationOptions` 字段。
    pub initialization_options: Option<serde_json::Value>,
    /// 单次 LSP request 超时,默认 30s。
    pub timeout: Duration,
}

impl TryFrom<LspServerConfigShape> for LspServerConfig {
    type Error = LspError;

    fn try_from(s: LspServerConfigShape) -> Result<Self, Self::Error> {
        let mut patterns = Vec::with_capacity(s.patterns.len());
        for p in &s.patterns {
            patterns.push(CompiledFilePattern::compile(
                &p.glob,
                p.language_id.clone(),
            )?);
        }
        let root_uri = match s.root_uri.as_deref() {
            None => None,
            Some(s) => Some(
                Url::parse(s)
                    .map_err(|e| LspError::ConfigInvalid(format!("root_uri {s:?} invalid: {e}")))?,
            ),
        };
        Ok(Self {
            name: s.name,
            command: s.command,
            args: s.args,
            env: s.env,
            patterns,
            root_uri,
            initialization_options: s.initialization_options,
            timeout: s.timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn shape(name: &str, patterns: &[(&str, &str)]) -> LspServerConfigShape {
        LspServerConfigShape {
            name: name.to_string(),
            command: "rust-analyzer".to_string(),
            args: vec![],
            env: HashMap::new(),
            patterns: patterns
                .iter()
                .map(|(g, l)| reflect_config::LspFilePattern {
                    glob: (*g).to_string(),
                    language_id: (*l).to_string(),
                })
                .collect(),
            root_uri: None,
            initialization_options: None,
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn compile_pattern_matches_rust_files() {
        let p = CompiledFilePattern::compile("**/*.rs", "rust").unwrap();
        assert!(p.glob.is_match(Path::new("src/main.rs")));
        assert!(!p.glob.is_match(Path::new("README.md")));
        assert_eq!(p.language_id, "rust");
    }

    #[test]
    fn try_from_compiles_all_patterns() {
        let s = shape("rust", &[("**/*.rs", "rust"), ("**/*.toml", "toml")]);
        let cfg: LspServerConfig = s.try_into().unwrap();
        assert_eq!(cfg.patterns.len(), 2);
        assert!(cfg.patterns[0].glob.is_match(Path::new("lib.rs")));
        assert!(!cfg.patterns[0].glob.is_match(Path::new("main.go")));
        assert!(cfg.patterns[1].glob.is_match(Path::new("Cargo.toml")));
    }

    #[test]
    fn invalid_glob_errors() {
        // `[` 是不闭合的 char class,glob 解析失败。
        let r = CompiledFilePattern::compile("[", "rust");
        assert!(matches!(r, Err(LspError::Glob(_))));
    }

    #[test]
    fn invalid_root_uri_errors() {
        let mut s = shape("rust", &[("**/*.rs", "rust")]);
        s.root_uri = Some("not a url".to_string());
        let r: Result<LspServerConfig, _> = s.try_into();
        assert!(matches!(r, Err(LspError::ConfigInvalid(_))));
    }
}
