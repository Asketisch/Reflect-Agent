//! `matcher` — pattern DSL 编译 + 搜索执行。
//!
//! v1 暴露 3 种前缀:
//! - `kind:<node_kind>` —— tree-sitter 手工 walk 收集所有匹配 `kind()` 的节点
//! - `regex:<re>` —— tree-sitter leaf walk + 在 leaf 文本上跑 `regex::Regex`
//! - `text:<literal>` —— 走 `ignore::WalkBuilder` + `String::contains`
//!   (无 AST 解析;按行输出 `path:line:line_content`)
//!
//! bare-pattern(`fn $NAME(...)`)留 v1+ —— 需要 metavar capture + rewrite plumbing。
//!
//! ## Hit 输出
//!
//! 每个命中是一个 `Hit { file, line, col, kind, text }`:
//! - `file`:相对 workspace 的路径
//! - `line` / `col`:1-indexed(`tree-sitter` 0-indexed,工具层 +1)
//! - `kind`:匹配的 node 类型(`kind:` 时 == pattern;`regex:` 时 == leaf kind)
//! - `text`:匹配所在行的源代码(trim trailing 空白)
//!
//! `Search` action 把 `Vec<Hit>` 序列化成 JSON 文本返回 `ContentBlock::text`。

use std::fs;
use std::path::Path;

use ignore::WalkBuilder;
use regex::Regex;
use serde::Serialize;
use tree_sitter::Tree;

use crate::grid::{AstError, LanguageGrid, LanguageId};

/// 编译后的 search pattern。
///
/// 三种模式完全独立,无 fallback:无前缀的 pattern 当作 `BadPattern` 报错
/// —— LLM 必须显式选 kind/regex/text 之一。
#[derive(Debug)]
pub enum CompiledPattern {
    /// `kind:<node_kind>` —— 树中所有 `kind == <node_kind>` 的节点。
    Kind(String),
    /// `regex:<re>` —— leaf 文本上跑 regex。
    Regex(Regex),
    /// `text:<literal>` —— 行级 `String::contains`。
    Text(String),
}

impl CompiledPattern {
    /// 编译原始 pattern 字符串。
    ///
    /// - `kind:<x>` → `Kind(x)`
    /// - `regex:<x>` → `Regex(Regex::new(x)?)`
    /// - `text:<x>` → `Text(x)`
    /// - 其他 → `AstError::BadPattern`
    pub fn compile(raw: &str) -> Result<Self, AstError> {
        let (prefix, body) = raw.split_once(':').ok_or_else(|| {
            AstError::BadPattern(format!(
                "missing prefix; expected kind:<node> | regex:<re> | text:<literal>, got {raw:?}"
            ))
        })?;
        let body = body.trim();
        if body.is_empty() {
            return Err(AstError::BadPattern(format!(
                "empty body for {prefix}: prefix"
            )));
        }
        match prefix {
            "kind" => Ok(Self::Kind(body.to_string())),
            "regex" => Regex::new(body)
                .map(Self::Regex)
                .map_err(|e| AstError::BadPattern(format!("regex compile: {e}"))),
            "text" => Ok(Self::Text(body.to_string())),
            other => Err(AstError::BadPattern(format!(
                "unknown prefix {other:?}; expected kind | regex | text"
            ))),
        }
    }
}

/// 单个 search 命中。`line` / `col` 1-indexed。
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    /// 相对 workspace 的文件路径(`text:` 模式下是绝对路径字符串化)。
    pub file: String,
    /// 1-indexed 行号。
    pub line: usize,
    /// 1-indexed 列号(`kind:` 时 0;`regex:` 时是 leaf 起点 col)。
    pub col: usize,
    /// 节点 kind(`kind:` / `regex:` 时填充;`text:` 时为空字符串)。
    pub kind: String,
    /// 匹配所在行源码(去掉 trailing 空白)。
    pub text: String,
}

/// 在已 parse 的 `Tree` 上跑 `CompiledPattern::Kind`。
///
/// tree-sitter 是 error-tolerant,即使有语法错误也产出 partial tree;这里
/// 不强制过滤 `ERROR` 节点(LLM 可能会主动想看 syntax error 范围),但
/// `kind:ERROR` 仍然能命中(LLM 显式选它就是想要)。
pub fn search_kind(tree: &Tree, source: &str, target_kind: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut cursor = tree.walk();
    walk_kind(&mut cursor, target_kind, source, &mut hits);
    hits
}

fn walk_kind(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    target: &str,
    source: &str,
    hits: &mut Vec<Hit>,
) {
    let node = cursor.node();
    if node.kind() == target {
        let start = node.start_position();
        let line_text = source
            .lines()
            .nth(start.row)
            .unwrap_or("")
            .trim_end()
            .to_string();
        hits.push(Hit {
            // call site 决定 file 字段(workspace 相对路径);这里先占位
            file: String::new(),
            line: start.row + 1,
            col: 0,
            kind: target.to_string(),
            text: line_text,
        });
    }
    if cursor.goto_first_child() {
        loop {
            walk_kind(cursor, target, source, hits);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// 在 `Tree` 的 leaf 节点上跑 regex。
///
/// `tree-sitter` leaf 指没有 named child 的节点(即 token);regex 在
/// leaf 文本上 is_match 即可。返回命中位置 = leaf 的 start_position。
pub fn search_regex(tree: &Tree, source: &str, re: &Regex) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut cursor = tree.walk();
    walk_regex(&mut cursor, re, source, &mut hits);
    hits
}

fn walk_regex(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    re: &Regex,
    source: &str,
    hits: &mut Vec<Hit>,
) {
    let node = cursor.node();
    // leaf = child_count() == 0;`let-chain` 把 utf8_text 校验和 regex
    // 命中合并到同一层 if,避免 `clippy::collapsible_if`。
    if node.child_count() == 0
        && let Ok(text) = node.utf8_text(source.as_bytes())
        && re.is_match(text)
    {
        let start = node.start_position();
        let line_text = source
            .lines()
            .nth(start.row)
            .unwrap_or("")
            .trim_end()
            .to_string();
        hits.push(Hit {
            file: String::new(),
            line: start.row + 1,
            col: start.column + 1,
            kind: node.kind().to_string(),
            text: line_text,
        });
    }
    if cursor.goto_first_child() {
        loop {
            walk_regex(cursor, re, source, hits);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// `text:` 模式硬屏蔽目录(对齐 `grep` builtin 的 `IGNORE_DIRS`):
/// 临时目录里通常没有 `.gitignore`,`standard_filters` 不会主动屏蔽
/// `target/` / `node_modules`,这里显式跳过。
const TEXT_IGNORE_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// 走 workspace 收集 `String::contains` 命中(`text:` 模式)。
///
/// `text:` 模式不解析 AST,只对每行做子串匹配;`Hit::kind` 留空。
/// `ignore::WalkBuilder` 自动尊重 `.gitignore`,并显式跳过
/// `target` / `node_modules` / `.git`(对齐 `grep` builtin)。
pub fn search_text(
    root: &Path,
    needle: &str,
    include: Option<&str>,
    max_results: usize,
) -> Result<Vec<Hit>, AstError> {
    if needle.is_empty() {
        return Err(AstError::BadPattern("text: needle is empty".into()));
    }
    let mut hits: Vec<Hit> = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !TEXT_IGNORE_DIRS.iter().any(|d| name == *d)
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(glob) = include {
            let path_s = path.to_string_lossy();
            let ext = glob.trim_start_matches("*.");
            if !path_s.ends_with(ext) && !path_s.ends_with(glob) {
                continue;
            }
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.contains(needle) {
                hits.push(Hit {
                    file: path.display().to_string(),
                    line: i + 1,
                    col: 0,
                    kind: String::new(),
                    text: line.trim_end().to_string(),
                });
                if hits.len() >= max_results {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

/// 统一 search 入口:`grid.parse(lang, source)` + 走对应分支。
///
/// `relative_file` 是 `text:` 模式下 hit 的 `file` 字段前缀(其他模式
/// 在调用方写入)。kind/regex 模式不依赖文件系统,只调 `parse` 拿 Tree。
pub fn search_in_tree(
    grid: &LanguageGrid,
    lang: LanguageId,
    source: &str,
    pattern: &CompiledPattern,
) -> Result<Vec<Hit>, AstError> {
    let tree = grid.parse(lang, source)?;
    Ok(match pattern {
        CompiledPattern::Kind(k) => search_kind(&tree, source, k),
        CompiledPattern::Regex(re) => search_regex(&tree, source, re),
        // text: 在调用方单独处理(需要 walk 文件系统)
        CompiledPattern::Text(_) => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> std::sync::Arc<LanguageGrid> {
        LanguageGrid::with_defaults()
    }

    #[test]
    fn compile_kind() {
        let p = CompiledPattern::compile("kind:function_item").unwrap();
        assert!(matches!(p, CompiledPattern::Kind(s) if s == "function_item"));
    }

    #[test]
    fn compile_regex() {
        let p = CompiledPattern::compile("regex:^fn ").unwrap();
        assert!(matches!(p, CompiledPattern::Regex(_)));
    }

    #[test]
    fn compile_text() {
        let p = CompiledPattern::compile("text:TODO").unwrap();
        match p {
            CompiledPattern::Text(s) => assert_eq!(s, "TODO"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn compile_rejects_missing_prefix() {
        let err = CompiledPattern::compile("function_item").unwrap_err();
        match err {
            AstError::BadPattern(m) => assert!(m.contains("missing prefix")),
            other => panic!("expected BadPattern, got {other:?}"),
        }
    }

    #[test]
    fn compile_rejects_unknown_prefix() {
        let err = CompiledPattern::compile("ast:function_item").unwrap_err();
        assert!(matches!(err, AstError::BadPattern(_)));
    }

    #[test]
    fn compile_rejects_empty_body() {
        let err = CompiledPattern::compile("kind:").unwrap_err();
        assert!(matches!(err, AstError::BadPattern(_)));
    }

    #[test]
    fn compile_rejects_invalid_regex() {
        let err = CompiledPattern::compile("regex:[").unwrap_err();
        assert!(matches!(err, AstError::BadPattern(_)));
    }

    #[test]
    fn search_kind_rust_finds_function_items() {
        let grid = grid();
        let src = "fn alpha() {}\nfn beta() {}\nstruct S;";
        let p = CompiledPattern::compile("kind:function_item").unwrap();
        let mut hits = search_in_tree(&grid, LanguageId("rust"), src, &p).unwrap();
        // 填充 file 字段(通常由 caller 完成)
        for h in &mut hits {
            h.file = "inline.rs".into();
        }
        assert_eq!(hits.len(), 2, "got: {hits:?}");
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].line, 2);
        assert!(hits[0].text.contains("alpha"));
        assert!(hits[1].text.contains("beta"));
    }

    #[test]
    fn search_kind_python_finds_function_definition() {
        let grid = grid();
        let src = "def alpha():\n    pass\ndef beta():\n    pass\n";
        let p = CompiledPattern::compile("kind:function_definition").unwrap();
        let hits = search_in_tree(&grid, LanguageId("python"), src, &p).unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
    }

    #[test]
    fn search_kind_tsx_finds_jsx_element() {
        let grid = grid();
        let src = "const x = <div>hi</div>;\nfunction f() { return <span/>; }";
        let p = CompiledPattern::compile("kind:jsx_element").unwrap();
        let hits = search_in_tree(&grid, LanguageId("tsx"), src, &p).unwrap();
        assert!(
            !hits.is_empty(),
            "expected at least one jsx_element, got: {hits:?}"
        );
    }

    #[test]
    fn search_kind_unknown_returns_empty() {
        let grid = grid();
        let src = "fn main() {}";
        let p = CompiledPattern::compile("kind:nonexistent_kind_xyz").unwrap();
        let hits = search_in_tree(&grid, LanguageId("rust"), src, &p).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_regex_matches_leaf_text() {
        let grid = grid();
        let src = "fn foo() {}\nfn bar() {}\nstruct S;\n";
        let p = CompiledPattern::compile("regex:^fn ").unwrap();
        let hits = search_in_tree(&grid, LanguageId("rust"), src, &p).unwrap();
        // 每个 `fn ` token 命中一次(line 1 + line 2)
        assert!(hits.len() >= 2, "got: {hits:?}");
    }

    #[test]
    fn search_text_walks_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "alpha\nTODO: do thing\nbeta").unwrap();
        std::fs::write(root.join("b.txt"), "TODO: also here").unwrap();
        let hits = search_text(root, "TODO", None, 100).unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
        // 验证 line 字段
        let a_hit = hits.iter().find(|h| h.file.ends_with("a.rs")).unwrap();
        assert_eq!(a_hit.line, 2);
    }

    #[test]
    fn search_text_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/ignored.rs"), "TODO: hidden").unwrap();
        std::fs::write(root.join("a.rs"), "TODO: visible").unwrap();
        let hits = search_text(root, "TODO", None, 100).unwrap();
        // target/ 应被 .gitignore 或 standard_filters 屏蔽
        assert_eq!(hits.len(), 1, "expected only a.rs, got: {hits:?}");
        assert!(hits[0].file.ends_with("a.rs"));
    }

    #[test]
    fn search_text_rejects_empty_needle() {
        let tmp = tempfile::tempdir().unwrap();
        let err = search_text(tmp.path(), "", None, 100).unwrap_err();
        assert!(matches!(err, AstError::BadPattern(_)));
    }

    #[test]
    fn search_kind_go_finds_function_declaration() {
        let grid = grid();
        let src = "package main\nfunc A() {}\nfunc B() {}\n";
        let p = CompiledPattern::compile("kind:function_declaration").unwrap();
        let hits = search_in_tree(&grid, LanguageId("go"), src, &p).unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
    }

    #[test]
    fn search_kind_typescript_finds_interface() {
        let grid = grid();
        let src = "interface A { x: number; }\ninterface B { y: string; }\n";
        let p = CompiledPattern::compile("kind:interface_declaration").unwrap();
        let hits = search_in_tree(&grid, LanguageId("typescript"), src, &p).unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
    }

    #[test]
    fn search_kind_javascript_finds_function() {
        let grid = grid();
        let src = "function foo() {}\nconst bar = function() {};\n";
        let p = CompiledPattern::compile("kind:function_declaration").unwrap();
        let hits = search_in_tree(&grid, LanguageId("javascript"), src, &p).unwrap();
        // function_declaration 只匹配第一个;function_expression 走另一种 kind
        assert!(!hits.is_empty(), "expected at least one, got: {hits:?}");
    }
}
