//! `edit` — 写盘逻辑 (P3)。
//!
//! 两个独立动作:
//! - `apply_replace` —— 按 tree-sitter 节点 start_byte/end_byte 倒序替换,
//!   产出 `similar::TextDiff` unified diff。
//! - `apply_rename` —— 单文件 word-boundary 正则替换(`\b<from>\b`)。不跨文件;
//!   AST-scoped 跨文件 rename 留 v1+。
//!
//! ## 倒序替换理由
//!
//! 替换节点的 byte range 后,后续节点的 byte range 会失效(因 source 长度
//! 变化)。如果按 start_byte 升序替换,后一次替换会因前一次替换的 offset 漂移
//! 命中错位。倒序替换则每次的 byte range 都指向尚未被修改的 source 段,语义
//! 干净。tree-sitter 提供的 `start_byte` / `end_byte` 在替换前一次性收齐,
//! 不需要回查 Tree。

use regex::Regex;
use similar::TextDiff;

use crate::grid::AstError;

/// 一次替换的字节范围(0-indexed,半开区间 `[start_byte, end_byte)`)。
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

/// 在 `source` 上按 `replacement` 替换每个 `[start, end)` 区间,返回新字符串。
///
/// **必须按 start 倒序调用**(调用方负责):见模块文档。
pub fn splice_replacements(source: &str, ranges: &[ByteRange], replacement: &str) -> String {
    if ranges.is_empty() {
        return source.to_string();
    }
    let mut sorted: Vec<ByteRange> = ranges.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.start));
    let mut out = String::with_capacity(source.len());
    let mut cursor = source.len();
    for r in sorted {
        // sanity:range 必须合法
        debug_assert!(r.start <= r.end, "invalid range {:?}", r);
        debug_assert!(
            r.end <= cursor,
            "non-monotonic replacement: {:?} cursor={}",
            r,
            cursor
        );
        if r.end > cursor || r.start > r.end {
            // 防御性 fallback:出错时直接返回原 source,让调用方看到 0 diff。
            return source.to_string();
        }
        out.insert_str(0, &source[r.end..cursor]);
        out.insert_str(0, replacement);
        cursor = r.start;
    }
    out.insert_str(0, &source[..cursor]);
    out
}

/// `Replace` action 核心逻辑:从 `tree` 收集所有 `kind == <target>` 节点的
/// byte range,按倒序应用 `replacement` 替换,产出 `(new_source, unified_diff)`。
///
/// 注:不写盘 —— 写盘由调用方(`AstTool::exec_replace`)负责,这样单元测试
/// 可独立验证 `apply_replace` 的纯函数行为。
pub fn apply_replace(
    source: &str,
    tree: &tree_sitter::Tree,
    target_kind: &str,
    replacement: &str,
) -> Result<(String, String), AstError> {
    let ranges = collect_node_ranges(tree, target_kind);
    if ranges.is_empty() {
        let empty_diff = TextDiff::from_lines(source, source);
        return Ok((source.to_string(), format!("{}", empty_diff.unified_diff())));
    }
    let new_source = splice_replacements(source, &ranges, replacement);
    let diff = TextDiff::from_lines(source, &new_source);
    let unified = format!("{}", diff.unified_diff());
    Ok((new_source, unified))
}

/// 走 `tree` 收集所有 `kind == <target>` 节点的 byte range。
///
/// `Node` 在 walk 期间借用 `tree`,但只要 tree 不被改,node 的 byte range
/// 就是 source 里的稳定索引。
fn collect_node_ranges(tree: &tree_sitter::Tree, target: &str) -> Vec<ByteRange> {
    let mut ranges = Vec::new();
    let mut cursor = tree.walk();
    walk_collect(&mut cursor, target, &mut ranges);
    ranges
}

fn walk_collect(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    target: &str,
    ranges: &mut Vec<ByteRange>,
) {
    let node = cursor.node();
    if node.kind() == target {
        ranges.push(ByteRange {
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }
    if cursor.goto_first_child() {
        loop {
            walk_collect(cursor, target, ranges);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// `RenameSymbol` action 核心逻辑:单文件 word-boundary 正则替换。
///
/// 用 `\b<from>\b` 强制整词匹配,避免 `foo` 匹配到 `foobar`;接受编译后
/// 的 `Regex`(caller 提供 `from`,这里临时编译)。返回 `(new_source,
/// unified_diff, replacement_count)`。
pub fn apply_rename(
    source: &str,
    from: &str,
    to: &str,
) -> Result<(String, String, usize), AstError> {
    if from.is_empty() {
        return Err(AstError::BadPattern("rename_symbol: from is empty".into()));
    }
    if from == to {
        return Ok((source.to_string(), String::new(), 0));
    }
    // `\b` word boundary 在 Rust regex 里就是字面 `\b`,对 identifier 字符
    // (a-zA-Z0-9_) 自动 word boundary 切分;对中文 unicode 标识符退化
    // —— 中文标识符留 v1+ 处理。
    let pattern = format!(r"\b{re}\b", re = regex::escape(from));
    let re =
        Regex::new(&pattern).map_err(|e| AstError::BadPattern(format!("rename regex: {e}")))?;
    let new_source = re.replace_all(source, to).into_owned();
    let count = re.find_iter(source).count();
    let diff = TextDiff::from_lines(source, &new_source);
    let unified = format!("{}", diff.unified_diff());
    Ok((new_source, unified, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::LanguageGrid;

    fn grid() -> std::sync::Arc<LanguageGrid> {
        LanguageGrid::with_defaults()
    }

    #[test]
    fn splice_replacements_empty_returns_source() {
        let s = "hello\n";
        let r = splice_replacements(s, &[], "X");
        assert_eq!(r, s);
    }

    #[test]
    fn splice_replacements_single_range() {
        let s = "hello world";
        let r = splice_replacements(s, &[ByteRange { start: 6, end: 11 }], "rust");
        assert_eq!(r, "hello rust");
    }

    #[test]
    fn splice_replacements_multiple_ranges_reverse_order() {
        // 模拟两个 fn 命中:第一次替换 "foo" → "bar",第二次替换 "baz" → "qux"
        // 倒序拼接后,语义等价于 "abc bar xyz qux"。
        let s = "abc foo xyz baz end";
        let r = splice_replacements(
            s,
            &[
                ByteRange { start: 4, end: 7 },   // "foo"
                ByteRange { start: 12, end: 15 }, // "baz"
            ],
            // 注意:这里测试 helper,实际 apply_replace 不会用同一个 replacement 替换两个不同子串
            "X",
        );
        // 倒序先替换 baz → X(得 "abc foo xyz X end"),再替换 foo → X(得 "abc X xyz X end")
        assert_eq!(r, "abc X xyz X end");
    }

    #[test]
    fn apply_replace_rust_function_items() {
        let g = grid();
        let src = "fn foo() {}\nfn bar() {}\nstruct S;\n";
        let tree = g.parse(crate::grid::LanguageId("rust"), src).unwrap();
        let (new, diff) = apply_replace(src, &tree, "function_item", "fn REPLACED() {}").unwrap();
        // 两个 fn_item 都被替换,struct 不动。
        assert!(!new.contains("fn foo"));
        assert!(!new.contains("fn bar"));
        assert!(new.contains("fn REPLACED"));
        assert!(new.contains("struct S"));
        // diff 包含 - / + 标记
        assert!(diff.starts_with("---") || diff.contains("-fn foo"));
        assert!(diff.contains("+fn REPLACED"));
    }

    #[test]
    fn apply_replace_no_match_returns_empty_diff() {
        let g = grid();
        let src = "fn foo() {}";
        let tree = g.parse(crate::grid::LanguageId("rust"), src).unwrap();
        let (new, _diff) = apply_replace(src, &tree, "nonexistent_kind", "X").unwrap();
        assert_eq!(new, src);
    }

    #[test]
    fn apply_rename_word_boundary_works() {
        let src = "let foo = 1;\nlet foobar = 2;\nlet xfoo = 3;\n";
        let (new, _diff, count) = apply_rename(src, "foo", "bar").unwrap();
        // 只替换整词 `foo`,foobar / xfoo 不动
        assert!(new.contains("let bar = 1"));
        assert!(new.contains("let foobar = 2"));
        assert!(new.contains("let xfoo = 3"));
        assert_eq!(count, 1);
    }

    #[test]
    fn apply_rename_multiple_occurrences() {
        let src = "foo foo foo";
        let (new, _diff, count) = apply_rename(src, "foo", "bar").unwrap();
        assert_eq!(new, "bar bar bar");
        assert_eq!(count, 3);
    }

    #[test]
    fn apply_rename_same_name_is_noop() {
        let src = "foo bar";
        let (new, _diff, count) = apply_rename(src, "foo", "foo").unwrap();
        assert_eq!(new, src);
        assert_eq!(count, 0);
    }

    #[test]
    fn apply_rename_rejects_empty_from() {
        let err = apply_rename("source", "", "to").unwrap_err();
        assert!(matches!(err, AstError::BadPattern(_)));
    }

    #[test]
    fn apply_rename_supports_underscore_identifier() {
        // \b 应该把 _ 当作 word 字符。
        let src = "let my_var = 1; let other_var = 2;";
        let (new, _diff, count) = apply_rename(src, "my_var", "renamed").unwrap();
        assert!(new.contains("let renamed = 1"));
        assert!(new.contains("let other_var = 2"));
        assert_eq!(count, 1);
    }
}
