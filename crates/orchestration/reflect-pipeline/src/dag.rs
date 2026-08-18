//! `dag` —— Pipeline DAG 层级编排(P2 `pipeline-dag`)。
//!
//! 把拓扑序按 level 分组,同 level 节点可并行执行(当前 stub 仍串行,
//! 但暴露 level 结构供 caller / 测试断言)。

use crate::error::PipelineError;
use crate::graph::DiGraph;

/// 按依赖 depth 把节点分到 level;level 0 = 无上游。
pub fn levels_from_topo(
    order: &[String],
    graph: &DiGraph,
) -> Result<Vec<Vec<String>>, PipelineError> {
    if order.is_empty() {
        return Ok(vec![]);
    }

    let mut depth: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for label in order {
        let mut max_up = 0usize;
        for other in graph.node_labels() {
            if graph.successors(other).iter().any(|s| s == label) {
                let d = depth.get(other).copied().unwrap_or(0);
                max_up = max_up.max(d + 1);
            }
        }
        depth.insert(label.clone(), max_up);
    }

    let max_level = depth.values().copied().max().unwrap_or(0);
    let mut levels = vec![Vec::new(); max_level + 1];
    for label in order {
        let lv = depth.get(label).copied().unwrap_or(0);
        levels[lv].push(label.clone());
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diamond_has_three_levels() {
        let mut g = DiGraph::new();
        for n in ["a", "b", "c", "d"] {
            g.add_node(n).unwrap();
        }
        g.add_edge("a", "b").unwrap();
        g.add_edge("a", "c").unwrap();
        g.add_edge("b", "d").unwrap();
        g.add_edge("c", "d").unwrap();
        let order = g.topo_sort().unwrap();
        let levels = levels_from_topo(&order, &g).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["a"]);
        assert_eq!(levels[2], vec!["d"]);
        assert_eq!(levels[1].len(), 2);
    }
}
