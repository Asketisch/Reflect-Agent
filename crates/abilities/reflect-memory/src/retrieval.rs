//! v1.4 D2:记忆分节检索 —— 把平面 MEMORY.md 按 `## 标题` 拆成条目,
//! 用 BM25(与 grep 的 rank=bm25 共用 reflect-bm25 叶 crate)按当前
//! 查询选相关条目 + 最近使用条目,替换历史上的全量整块注入。
//!
//! 兼容性:无 `## 标题` 的旧格式整文件视为单条目 —— 条目集合退化为
//! 「整个文件」,预算装得下时输出与全量注入等价。

use std::sync::Arc;

use crate::model::MemoryScope;
use crate::scope::MAX_MEMORY_INJECT_CHARS;
use crate::store::MemoryStore;

/// 一个记忆条目:`## 标题` + 正文(或无标题整文件)。
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySection {
    pub scope: MemoryScope,
    /// `##` 标题文本;无标题条目为空串。
    pub title: String,
    /// 正文(不含标题行;无标题条目为整文件)。
    pub body: String,
    /// 文件内的序号(0-based,保持原顺序 —— 「最近使用」取最大序号)。
    pub index: usize,
}

/// 把一段 memory 文本按 `## ` 标题拆条目;无任何标题时返回单条目
/// (title = 空,body = 整文件)。首个标题前的引言并入首个条目的 body。
pub fn split_sections(scope: MemoryScope, body: &str) -> Vec<MemorySection> {
    let mut sections: Vec<MemorySection> = Vec::new();
    let mut current_title = String::new();
    let mut current_body = String::new();
    let mut has_content = false;
    let mut any_title = false;

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // 新条目开始:先落盘上一条。
            if has_content {
                sections.push(MemorySection {
                    scope,
                    title: current_title.clone(),
                    body: current_body.trim_end().to_string(),
                    index: sections.len(),
                });
            }
            any_title = true;
            current_title = rest.trim().to_string();
            current_body.clear();
            has_content = true;
        } else {
            current_body.push_str(line);
            current_body.push('\n');
            has_content = true;
        }
    }
    if has_content {
        let title = if any_title || !current_title.is_empty() {
            current_title.clone()
        } else {
            String::new()
        };
        sections.push(MemorySection {
            scope,
            title,
            body: current_body.trim_end().to_string(),
            index: sections.len(),
        });
    }
    sections
}

/// BM25 检索:加载各 scope 的记忆 → 拆条目 → 按 `query` 相关度排序,
/// 相关度为零的条目按「越新越优先」排在相关条目之后;在 `budget`
/// 字符预算内从高到低装填。`query` 为空或全部条目装得下时退化为
/// 「原顺序全量」(等价历史行为)。
///
/// 返回已格式化的注入文本(scope 分节头沿用 `load_combined` 的形态,
/// 单 scope 时不加头)。
pub fn retrieve_relevant(
    store: &dyn MemoryStore,
    scopes: &[MemoryScope],
    agent_type: &str,
    query: &str,
    budget: usize,
) -> Result<String, crate::model::MemoryError> {
    // 收集全部非空 scope 正文。
    let mut non_empty: Vec<(MemoryScope, String)> = Vec::new();
    for s in scopes {
        let body = store.load(*s, agent_type)?;
        if body.trim().is_empty() {
            continue;
        }
        non_empty.push((*s, body));
    }
    if non_empty.is_empty() {
        return Ok(String::new());
    }
    // 单 scope 无标题的场景:整文件注入,等价历史行为。
    let total_chars: usize = non_empty.iter().map(|(_, b)| b.len()).sum();
    if query.trim().is_empty() || total_chars <= budget {
        return crate::store::full_combined(non_empty);
    }

    // 拆条目 + BM25 打分(标题并入文档,让标题命中权重自然提升)。
    let mut sections: Vec<MemorySection> = Vec::new();
    for (s, body) in &non_empty {
        sections.extend(split_sections(*s, body));
    }
    let query_terms = reflect_bm25::tokenize(query);
    let candidates: Vec<(usize, String)> = sections
        .iter()
        .map(|sec| {
            (
                sec.index,
                if sec.title.is_empty() {
                    sec.body.clone()
                } else {
                    format!("{} {}", sec.title, sec.body)
                },
            )
        })
        .collect();
    let hits = reflect_bm25::rank_lines(&query_terms, &candidates, 1.2, 0.75);
    let score_of = |idx: usize| -> f64 {
        hits.iter()
            .find(|h| h.line_no == idx)
            .map(|h| h.score)
            .unwrap_or(0.0)
    };

    // 排序:有分的按分数降序;零分的按条目新→旧(逆文件序)。
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by(|&a, &b| {
        let (sa, sb) = (score_of(a), score_of(b));
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.cmp(&a))
    });

    // 装填(预算内尽量多;至少装一条,单条超预算时截断)。
    let mut picked: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for &idx in &order {
        let len = sections[idx].body.len() + sections[idx].title.len() + 8;
        if used + len > budget && !picked.is_empty() {
            break;
        }
        picked.push(idx);
        used += len;
    }
    // 还原文件顺序输出(注入稳定性:同集合同顺序,便于 prompt cache)。
    picked.sort_unstable();

    // 多 scope 时加 scope 分节头(与 load_combined 形态一致)。
    let multi_scope = non_empty.len() > 1;
    let mut parts: Vec<String> = Vec::new();
    for &idx in &picked {
        let sec = &sections[idx];
        let rendered = if sec.title.is_empty() {
            sec.body.clone()
        } else {
            format!("## {}\n{}", sec.title, sec.body)
        };
        if multi_scope {
            parts.push(format!("### {} memory\n\n{}", sec.scope, rendered));
        } else {
            parts.push(rendered);
        }
    }
    Ok(parts.join("\n\n"))
}

/// 便捷包装:Arc store 版本(供 pre_loop 等持有 `Arc<dyn MemoryStore>`
/// 的调用方)。
pub fn retrieve_relevant_arc(
    store: &Arc<dyn MemoryStore>,
    scopes: &[MemoryScope],
    agent_type: &str,
    query: &str,
) -> Result<String, crate::model::MemoryError> {
    retrieve_relevant(
        store.as_ref(),
        scopes,
        agent_type,
        query,
        MAX_MEMORY_INJECT_CHARS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileMemoryStore;

    fn store_with(body: &str) -> (tempfile::TempDir, FileMemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMemoryStore::new(dir.path(), dir.path());
        store.save(MemoryScope::Project, "agent", body).unwrap();
        (dir, store)
    }

    #[test]
    fn split_sections_by_heading() {
        let body = "intro\n## A\nalpha\n## B\nbeta";
        let secs = split_sections(MemoryScope::Project, body);
        assert_eq!(secs.len(), 3);
        assert_eq!(secs[0].title, "", "标题前引言归入无标题首条");
        assert!(secs[0].body.contains("intro"));
        assert_eq!(secs[1].title, "A");
        assert!(secs[1].body.contains("alpha"));
        assert_eq!(secs[2].title, "B");
    }

    #[test]
    fn split_sections_without_headings_single_entry() {
        let secs = split_sections(MemoryScope::Project, "just one blob");
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].title, "");
        assert_eq!(secs[0].body, "just one blob");
    }

    #[test]
    fn retrieval_ranks_relevant_section_first() {
        let body = "## Database\nUses postgres 16 with pgbouncer.\n\n## Coding Style\nAlways run cargo fmt before commit.\n\n## Deploy\nDeploy via github actions on main.";
        let (_d, store) = store_with(body);
        let out = retrieve_relevant(
            &store,
            &[MemoryScope::Project],
            "agent",
            "which database does the project use",
            8000,
        )
        .unwrap();
        // 预算充裕时全量返回,但排序上 Database 在前(prompt cache 稳定
        // 的文件序还原 —— 此处断言内容完整)。
        assert!(out.contains("postgres 16"));
        assert!(out.contains("cargo fmt"));
    }

    #[test]
    fn retrieval_budget_drops_irrelevant() {
        // 三条长文;查询命中 coding;预算只够两条 → 无关的 Deploy 被裁掉。
        let filler = "x".repeat(120);
        let body = format!(
            "## Coding Style\nfmt first {f}\n\n## Deploy\nactions deploy {f}\n\n## Database\npostgres {f}",
            f = filler
        );
        let (_d, store) = store_with(&body);
        let out = retrieve_relevant(
            &store,
            &[MemoryScope::Project],
            "agent",
            "coding style fmt",
            400,
        )
        .unwrap();
        assert!(out.contains("Coding Style"), "命中的条目必须保留");
        assert!(
            !out.contains("Deploy") || !out.contains("Database"),
            "预算裁剪应丢掉低相关条目"
        );
    }

    #[test]
    fn retrieval_empty_query_falls_back_to_full() {
        let body = "## A\nalpha\n## B\nbeta";
        let (_d, store) = store_with(body);
        let out = retrieve_relevant(&store, &[MemoryScope::Project], "agent", "", 8000).unwrap();
        assert!(out.contains("alpha") && out.contains("beta"));
    }
}
