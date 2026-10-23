//! BM25 相关性排序 —— 供 `grep` 工具的 `rank=bm25` 模式使用。
//!
//! 依赖 `rankfns` 提供的 Okapi BM25 核函数;本模块负责分词、语料统计与
//! 候选行打分,不涉及倒排索引(符合 v1 P1 的「ripgrep + BM25 排序」范围)。

use rankfns::{bm25_idf_plus1, bm25_tf};

/// 单条 BM25 命中:分数 + 原始行文本 + 1-based 行号。
#[derive(Debug, Clone, PartialEq)]
pub struct Bm25Hit {
    pub score: f64,
    pub line_no: usize,
    pub line: String,
}

/// 将查询与文本规范化为 BM25 词项(小写、按非字母数字切分)。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

/// 对候选行集合按 BM25 打分并降序排列。
///
/// `query_terms` 为空时返回空 Vec。`candidates` 每项为 `(line_no, line_text)`。
pub fn rank_lines(
    query_terms: &[String],
    candidates: &[(usize, String)],
    k1: f64,
    b: f64,
) -> Vec<Bm25Hit> {
    if query_terms.is_empty() || candidates.is_empty() {
        return Vec::new();
    }

    let k1 = k1 as f32;
    let b = b as f32;
    let n_docs = candidates.len();
    let avg_dl = candidates
        .iter()
        .map(|(_, line)| tokenize(line).len())
        .sum::<usize>() as f32
        / n_docs as f32;

    // 每个 query term 在多少篇「文档」(行) 里出现过。
    let mut df = vec![0usize; query_terms.len()];
    let mut doc_tokens: Vec<Vec<String>> = Vec::with_capacity(n_docs);
    for (_, line) in candidates {
        let tokens = tokenize(line);
        for (i, term) in query_terms.iter().enumerate() {
            if tokens.iter().any(|t| t == term) {
                df[i] += 1;
            }
        }
        doc_tokens.push(tokens);
    }

    let mut scored: Vec<Bm25Hit> = candidates
        .iter()
        .enumerate()
        .map(|(doc_idx, (line_no, line))| {
            let dl = doc_tokens[doc_idx].len() as f32;
            let mut score = 0.0_f64;
            for (i, term) in query_terms.iter().enumerate() {
                let tf = doc_tokens[doc_idx].iter().filter(|t| *t == term).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                let idf = bm25_idf_plus1(n_docs as u32, df[i] as u32);
                score += f64::from(idf * bm25_tf(tf, dl, avg_dl, k1, b));
            }
            Bm25Hit {
                score,
                line_no: *line_no,
                line: line.clone(),
            }
        })
        .filter(|h| h.score > 0.0)
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_non_alnum() {
        assert_eq!(
            tokenize("Hello World_foo-bar"),
            vec!["hello", "world_foo", "bar"]
        );
    }

    #[test]
    fn rank_lines_orders_by_relevance() {
        let query = tokenize("async fn main");
        let candidates = vec![
            (1, "let x = 1;".to_string()),
            (2, "async fn main() { println!(\"hi\"); }".to_string()),
            (3, "async fn main() { do_work().await; }".to_string()),
        ];
        let ranked = rank_lines(&query, &candidates, 1.2, 0.75);
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].score >= ranked[1].score);
        assert!(ranked.iter().any(|h| h.line_no == 2));
        assert!(ranked.iter().any(|h| h.line_no == 3));
    }

    #[test]
    fn rank_lines_empty_query_returns_empty() {
        assert!(rank_lines(&[], &[(1, "x".into())], 1.2, 0.75).is_empty());
    }
}
