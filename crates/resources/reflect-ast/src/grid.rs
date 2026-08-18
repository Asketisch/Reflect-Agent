//! `LanguageGrid` — extension → `tree_sitter::Language` 注册表。
//!
//! 启动期根据 Cargo features 把每个 enabled grammar 的
//! `tree_sitter_<lang>::LANGUAGE` 装进 `Vec<LanguageSpec>`,再派生一个
//! `HashMap<ext, LanguageId>` 用于路径 → grammar 路由。
//!
//! ## 与 LSP 的对比
//!
//! - LSP 的 `pick_server_for` 在 stdio 子进程里 spawn 一个 server,通过
//!   glob 决定路由;`LanguageGrid::for_ext` 走纯进程内 extension 匹配。
//! - LSP 每 server 维护独立 peer(并发 / cancel 维度);`LanguageGrid`
//!   是无状态注册表,parser 由 `grid.parse()` 每次新建(P1+ 后续可加
//!   进程内 `DashMap<PathBuf, Tree>` 缓存)。
//!
//! ## 错误模型
//!
//! 唯一公开错误类型 `AstError`,经 `?` 透传到 `AstTool::execute` 转
//! `ToolError`。P0 阶段仅在 `grid.parse()` 路径上 trace。

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;
use tree_sitter::{Language, Parser, Tree};

/// Grammar 标识。`&'static str` —— 与 `LanguageSpec::id` 同生命周期。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LanguageId(pub &'static str);

impl LanguageId {
    /// 便于在日志 / tool output 里展示的字符串视图。
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

/// 单 grammar 注册条目。
///
/// `language` 是零参 closure 调用,返回 `tree_sitter::Language`。每个
/// grammar crate 暴露一个 `pub const LANGUAGE: LanguageFn`,配 `.into()`
/// 转 `Language`,所以这里 closure 体保持 `|| ...LANGUAGE.into()` 形态。
#[derive(Clone)]
pub struct LanguageSpec {
    pub id: LanguageId,
    /// 文件扩展名列表(不带前导 `.`)。匹配时统一 strip 前缀。
    pub extensions: &'static [&'static str],
    pub language: fn() -> Language,
}

/// AST 工具的错误类型。
///
/// v1 阶段仅暴露 4 类:`UnsupportedLanguage` / `Parse` / `BadPattern` /
/// `Io`。`AstTool::execute` 把它们转 `ToolError::InvalidArgs` 或
/// `ToolError::Execution` / `Io`,具体映射在 tool.rs 里。
#[derive(Debug, Error)]
pub enum AstError {
    /// 路径后缀没有匹配的 grammar(例如 `foo.xyz`)。
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    /// tree-sitter parse 失败(实际几乎不会触发,tree-sitter 是 error-
    /// tolerant 的,partial tree 也能 walk)。
    #[error("parse: {0}")]
    Parse(String),
    /// 模式字符串非法(非三 prefix 之一,或 regex 编译失败)。
    #[error("bad pattern: {0}")]
    BadPattern(String),
    /// IO 错(读源文件、写回等)。
    #[error("io: {0}")]
    Io(String),
}

/// Grammar 注册表。
///
/// 进程内单例(`with_defaults()` 返回 `Arc<Self>`)。`for_ext` 在
/// `AstTool::execute` 路径上 O(1) 路由;`parse` 每次新建 `Parser`(无
/// 状态,P1+ 再考虑 cache)。
pub struct LanguageGrid {
    specs: Vec<LanguageSpec>,
    by_ext: HashMap<String, LanguageId>,
}

impl LanguageGrid {
    /// 进程内默认 build 的 grammar 集合 —— 根据 Cargo features 编译期
    /// 决定。`#[cfg(feature = "lang-xxx")]` 守卫让未启用的 grammar 不
    /// 进 `specs`,`for_ext` 直接返回 `None`,`list` 不列出。
    ///
    /// TypeScript 的 `.ts` 走 `LANGUAGE_TYPESCRIPT`,`.tsx` 走
    /// `LANGUAGE_TSX`(两个 grammar 略有差异,JSX 仅在 TSX 中)。JavaScript
    /// 走 `LANGUAGE`。
    pub fn with_defaults() -> std::sync::Arc<Self> {
        let mut specs: Vec<LanguageSpec> = Vec::new();

        #[cfg(feature = "lang-rust")]
        specs.push(LanguageSpec {
            id: LanguageId("rust"),
            extensions: &["rs"],
            language: || tree_sitter_rust::LANGUAGE.into(),
        });

        #[cfg(feature = "lang-typescript")]
        {
            // `.ts` → typescript(无 JSX);`.tsx` → TSX(含 JSX)。
            specs.push(LanguageSpec {
                id: LanguageId("typescript"),
                extensions: &["ts"],
                language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            });
            specs.push(LanguageSpec {
                id: LanguageId("tsx"),
                extensions: &["tsx"],
                language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
            });
        }

        #[cfg(feature = "lang-python")]
        specs.push(LanguageSpec {
            id: LanguageId("python"),
            extensions: &["py"],
            language: || tree_sitter_python::LANGUAGE.into(),
        });

        #[cfg(feature = "lang-go")]
        specs.push(LanguageSpec {
            id: LanguageId("go"),
            extensions: &["go"],
            language: || tree_sitter_go::LANGUAGE.into(),
        });

        #[cfg(feature = "lang-javascript")]
        specs.push(LanguageSpec {
            id: LanguageId("javascript"),
            extensions: &["js", "mjs", "cjs"],
            language: || tree_sitter_javascript::LANGUAGE.into(),
        });

        let mut by_ext: HashMap<String, LanguageId> = HashMap::new();
        for spec in &specs {
            for ext in spec.extensions {
                by_ext.insert((*ext).to_ascii_lowercase(), spec.id);
            }
        }

        std::sync::Arc::new(Self { specs, by_ext })
    }

    /// 按文件后缀查 grammar ID。
    ///
    /// 大小写不敏感(扩展名统一 lowercase)。无扩展名或未在注册表里
    /// 时返回 `None`,由调用方决定转 `AstError::UnsupportedLanguage` 或
    /// 走 fallback 路径。
    pub fn for_ext(&self, path: &Path) -> Option<LanguageId> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_ext.get(&ext).copied()
    }

    /// 列出当前 build 启用的所有 grammar ID(P0 的 `list_languages`
    /// action 用)。
    pub fn list(&self) -> Vec<&'static str> {
        self.specs.iter().map(|s| s.id.0).collect()
    }

    /// 解析源代码为 tree-sitter `Tree`。P0 每次新建 `Parser`,无缓存
    /// —— 后续 P1+ 加 `DashMap<PathBuf, Tree>` 复用。
    pub fn parse(&self, lang: LanguageId, source: &str) -> Result<Tree, AstError> {
        let spec = self
            .specs
            .iter()
            .find(|s| s.id == lang)
            .ok_or_else(|| AstError::UnsupportedLanguage(lang.0.to_string()))?;
        let mut parser = Parser::new();
        parser
            .set_language(&(spec.language)())
            .map_err(|e| AstError::Parse(format!("set_language: {e}")))?;
        parser
            .parse(source, None)
            .ok_or_else(|| AstError::Parse("parser returned None".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_defaults_lists_expected_languages() {
        // 默认 features 启用 5 个 grammar:rust + typescript(含 tsx) +
        // python + go + javascript。所以 list 长度 = 6(ts 与 tsx 两条
        // LanguageSpec)。
        let grid = LanguageGrid::with_defaults();
        let langs = grid.list();
        assert!(langs.contains(&"rust"), "missing rust in {langs:?}");
        assert!(
            langs.contains(&"typescript"),
            "missing typescript in {langs:?}"
        );
        assert!(langs.contains(&"tsx"), "missing tsx in {langs:?}");
        assert!(langs.contains(&"python"), "missing python in {langs:?}");
        assert!(langs.contains(&"go"), "missing go in {langs:?}");
        assert!(
            langs.contains(&"javascript"),
            "missing javascript in {langs:?}"
        );
    }

    #[test]
    fn for_ext_is_case_insensitive() {
        let grid = LanguageGrid::with_defaults();
        assert_eq!(grid.for_ext(Path::new("foo.RS")).unwrap().0, "rust");
        assert_eq!(grid.for_ext(Path::new("foo.Py")).unwrap().0, "python");
        assert_eq!(grid.for_ext(Path::new("FOO/Bar.TSX")).unwrap().0, "tsx");
    }

    #[test]
    fn for_ext_returns_none_for_unknown() {
        let grid = LanguageGrid::with_defaults();
        assert!(grid.for_ext(Path::new("foo.xyz")).is_none());
        assert!(grid.for_ext(Path::new("Makefile")).is_none());
    }

    #[test]
    fn parse_rust_source_succeeds() {
        let grid = LanguageGrid::with_defaults();
        let src = "fn main() { println!(\"hi\"); }";
        let tree = grid
            .parse(LanguageId("rust"), src)
            .expect("parse rust source");
        let root = tree.root_node();
        assert_eq!(root.kind(), "source_file");
        // 至少有一个 fn_item child。
        assert!(
            root.child(0).is_some(),
            "expected at least one child node, got: {root:?}"
        );
    }

    #[test]
    fn parse_unsupported_language_errors() {
        let grid = LanguageGrid::with_defaults();
        let err = grid.parse(LanguageId("cobol"), "fn main() {}").unwrap_err();
        assert!(matches!(err, AstError::UnsupportedLanguage(_)));
    }
}
