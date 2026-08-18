//! 极简 `DiGraph` —— 自实现,避免引入 `petgraph` 外部依赖。
//!
//! 设计动机:`reflect-pipeline` 仅需四件套 —— `add_node` / `add_edge` /
//! `topo_sort` / `cycle_detect`。`petgraph` 是 ~5000 行通用图库 + ~50KB
//! 编译开销,本 crate 自实现 ~120 行足够,且 `Cargo.lock` 不会多一个 crate。
//!
//! # 数据布局
//!
//! 节点用 `Vec<NodeId>` 保持插入顺序,边用 `Vec<(NodeId, NodeId)>` 维持原序。
//! 不做邻接表优化 —— pipeline 通常 5-20 节点,`O(V+E)` 全扫足够。
//!
//! # 拓扑排序
//!
//! 走 Kahn's algorithm:统计入度 → 反复移除入度为 0 的节点 → 入度变 0 时入队。
//! 若最终 visited 节点数 < 总节点数,剩余节点必然成环,触发 `Cyclic` 错误。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::error::PipelineError;

/// 节点 id —— 在 `DiGraph` 内部的稳定索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) usize);

/// 有向图,节点标签 = `String`。
#[derive(Debug, Default, Clone)]
pub struct DiGraph {
    /// 节点列表(保持插入顺序),key = label, value = 内部 NodeId。
    label_to_id: HashMap<String, NodeId>,
    /// 节点 id → label(反向索引,O(1) 查表)。
    id_to_label: Vec<String>,
    /// 边列表(u, v) 表示 u → v(u 在前,v 在后;v 依赖 u 的输出)。
    edges: Vec<(NodeId, NodeId)>,
    /// 邻接表(出边)—— 为 `topo_sort` 的入度计算与 BFS 提供 O(1) 邻接。
    /// 重建时机:`add_node` / `add_edge` 之后,`rebuild_adjacency` 调一次。
    adjacency: Vec<Vec<NodeId>>,
    /// 邻接表是否已构建(`add_edge` / `add_node` 触发 dirty 标记)。
    adjacency_dirty: bool,
}

impl DiGraph {
    /// 新建空图。
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加节点。重复 label 报 `PipelineError::DuplicateNode`。
    pub fn add_node(&mut self, label: impl Into<String>) -> Result<NodeId, PipelineError> {
        let label = label.into();
        if self.label_to_id.contains_key(&label) {
            return Err(PipelineError::DuplicateNode(label));
        }
        let id = NodeId(self.id_to_label.len());
        self.label_to_id.insert(label.clone(), id);
        self.id_to_label.push(label);
        self.adjacency.push(Vec::new());
        self.adjacency_dirty = true;
        Ok(id)
    }

    /// 添加有向边 `from → to`(即 `to` 依赖 `from`)。
    ///
    /// `from` 或 `to` 不存在报 `PipelineError::UnknownNode`。
    /// 重复边静默允许(`add_edge` 不查重,符合 `DiGraph` 多重边语义)。
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<(), PipelineError> {
        let from_id = *self
            .label_to_id
            .get(from)
            .ok_or_else(|| PipelineError::UnknownNode(from.to_string()))?;
        let to_id = *self
            .label_to_id
            .get(to)
            .ok_or_else(|| PipelineError::UnknownNode(to.to_string()))?;
        self.edges.push((from_id, to_id));
        self.adjacency_dirty = true;
        Ok(())
    }

    /// 节点数量。
    pub fn node_count(&self) -> usize {
        self.id_to_label.len()
    }

    /// 边数量。
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 按 label 查 id。
    pub fn id_of(&self, label: &str) -> Option<NodeId> {
        self.label_to_id.get(label).copied()
    }

    /// 按 id 查 label。
    pub fn label_of(&self, id: NodeId) -> Option<&str> {
        self.id_to_label.get(id.0).map(String::as_str)
    }

    /// 列出所有节点 label(按插入顺序)。
    pub fn node_labels(&self) -> Vec<&str> {
        self.id_to_label.iter().map(String::as_str).collect()
    }

    /// 返回直接依赖 `label` 的下游节点 label 列表(出边)。
    /// 空图 / 不存在节点 → 空 Vec。
    pub fn successors_of(&self, label: &str) -> Vec<String> {
        // clone + rebuild adjacency 路径,与 `successors()` 同语义。
        // 节点数 < 100,clone 成本可忽略;避免引入 `RefCell`。
        let mut cloned = self.clone();
        cloned.ensure_adjacency();
        let Some(&id) = cloned.label_to_id.get(label) else {
            return Vec::new();
        };
        cloned.adjacency[id.0]
            .iter()
            .map(|nid| cloned.id_to_label[nid.0].clone())
            .collect()
    }

    /// 拓扑排序(Kahn's algorithm)。
    ///
    /// 返回节点 label 列表,顺序 = 可执行顺序(被依赖的节点先执行)。
    /// 存在环时报 `PipelineError::Cyclic` —— 错误信息携带一个环上节点 label。
    pub(crate) fn topo_sorted(&mut self) -> Result<Vec<String>, PipelineError> {
        self.ensure_adjacency();
        if self.id_to_label.is_empty() {
            return Ok(Vec::new());
        }

        // 入度计算:基于边列表,与邻接表无关。
        let mut in_degree = vec![0usize; self.id_to_label.len()];
        for &(_, to) in &self.edges {
            in_degree[to.0] += 1;
        }

        // 起始队列:入度 0 的所有节点;用 BTreeMap 保证稳定顺序(label 字典序)。
        let mut ready: BTreeMap<String, NodeId> = BTreeMap::new();
        for (label, &id) in &self.label_to_id {
            if in_degree[id.0] == 0 {
                ready.insert(label.clone(), id);
            }
        }

        let mut order = Vec::with_capacity(self.id_to_label.len());
        let mut popped = HashSet::new();
        while let Some((label, id)) = ready.iter().next().map(|(k, v)| (k.clone(), *v)) {
            ready.remove(&label);
            order.push(label.clone());
            popped.insert(id);
            for &succ in &self.adjacency[id.0] {
                in_degree[succ.0] -= 1;
                if in_degree[succ.0] == 0 {
                    let succ_label = self.id_to_label[succ.0].clone();
                    if !popped.contains(&succ) && !ready.contains_key(&succ_label) {
                        ready.insert(succ_label, succ);
                    }
                }
            }
        }

        if order.len() != self.id_to_label.len() {
            let stuck: Vec<String> = self
                .id_to_label
                .iter()
                .enumerate()
                .filter(|(i, _)| in_degree[*i] > 0)
                .map(|(_, l)| l.clone())
                .collect();
            let pivot = stuck
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            Err(PipelineError::Cyclic(pivot))
        } else {
            Ok(order)
        }
    }

    /// `&self` 入口的拓扑排序 —— `clone()` 后 mutate(pipeline 节点数 < 100
    /// 时成本可忽略)。
    pub fn topo_sort(&self) -> Result<Vec<String>, PipelineError> {
        let mut cloned = self.clone();
        cloned.topo_sorted()
    }

    /// 按拓扑**层级**分组(P2 `pipeline-dag` 同层并行 fan-out 用)。
    ///
    /// 返回 `Vec<Vec<String>>`,每个内层 Vec = 同一层级(入度同深度)的节点。
    /// 同层节点之间无依赖关系,可并行执行;层级间严格顺序执行。
    /// 例:钻石 `a → b → d, a → c → d` → `[[a], [b, c], [d]]`。
    ///
    /// 同层内顺序按 label 字典序(Kahn 用 BTreeMap,确定性)。存在环报 `Cyclic`。
    pub fn topo_levels(&self) -> Result<Vec<Vec<String>>, PipelineError> {
        let order = self.topo_sort()?;
        if order.is_empty() {
            return Ok(Vec::new());
        }
        // level(node) = 0 若无上游,否则 = max(level(上游)) + 1。
        let mut level: HashMap<String, usize> = HashMap::new();
        // 用拓扑序保证赋值 level 时上游已就绪。
        for label in &order {
            let preds = self.predecessors(label);
            let l = preds
                .iter()
                .map(|p| level.get(p).copied().unwrap_or(0))
                .max()
                .map(|m| m + 1)
                .unwrap_or(0);
            level.insert(label.clone(), l);
        }
        let max_level = level.values().copied().max().unwrap_or(0);
        let mut levels: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];
        for label in &order {
            let l = level[label];
            levels[l].push(label.clone());
        }
        // 每层内按字典序(Kahn 已字典序,这里二次保证)。
        for layer in &mut levels {
            layer.sort();
        }
        Ok(levels)
    }

    /// 返回直接上游(入边源)节点 label 列表(给 `topo_levels` 用)。
    pub fn predecessors(&self, label: &str) -> Vec<String> {
        self.edges
            .iter()
            .filter_map(|(from, to)| {
                if self.id_to_label.get(to.0).map(String::as_str) == Some(label) {
                    self.id_to_label.get(from.0).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    /// `&self` 入口的 successors 查询 —— 内部 clone 后 rebuild。
    pub fn successors(&self, label: &str) -> Vec<String> {
        let mut cloned = self.clone();
        cloned.ensure_adjacency();
        let Some(&id) = cloned.label_to_id.get(label) else {
            return Vec::new();
        };
        cloned.adjacency[id.0]
            .iter()
            .map(|nid| cloned.id_to_label[nid.0].clone())
            .collect()
    }

    /// 惰性重建邻接表。`topo_sorted` 入口先调一次。
    fn ensure_adjacency(&mut self) {
        if !self.adjacency_dirty {
            return;
        }
        // 重置所有邻接列表。
        for v in &mut self.adjacency {
            v.clear();
        }
        for &(from, to) in &self.edges {
            self.adjacency[from.0].push(to);
        }
        self.adjacency_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_topo_is_empty() {
        let g = DiGraph::new();
        assert_eq!(g.topo_sort().unwrap(), Vec::<String>::new());
    }

    /// 单节点图:直接返回。
    #[test]
    fn single_node_topo() {
        let mut g = DiGraph::new();
        g.add_node("only").unwrap();
        assert_eq!(g.topo_sort().unwrap(), vec!["only".to_string()]);
    }

    /// 4 节点链 a → b → c → d:顺序固定。
    #[test]
    fn chain_topo_is_stable() {
        let mut g = DiGraph::new();
        for n in ["a", "b", "c", "d"] {
            g.add_node(n).unwrap();
        }
        g.add_edge("a", "b").unwrap();
        g.add_edge("b", "c").unwrap();
        g.add_edge("c", "d").unwrap();
        assert_eq!(
            g.topo_sort().unwrap(),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        );
    }

    /// 钻石拓扑:a → b → d,a → c → d。`b` 与 `c` 字典序排先(同入度 0 时
    /// Kahn 算法用 `BTreeMap`,确定性 + 稳定)。
    #[test]
    fn diamond_topo_is_deterministic() {
        let mut g = DiGraph::new();
        for n in ["a", "b", "c", "d"] {
            g.add_node(n).unwrap();
        }
        g.add_edge("a", "b").unwrap();
        g.add_edge("a", "c").unwrap();
        g.add_edge("b", "d").unwrap();
        g.add_edge("c", "d").unwrap();
        let order = g.topo_sort().unwrap();
        assert_eq!(order.first().map(String::as_str), Some("a"));
        assert_eq!(order.last().map(String::as_str), Some("d"));
        // b, c 都在 a 之后、d 之前。
        let pos_b = order.iter().position(|x| x == "b").unwrap();
        let pos_c = order.iter().position(|x| x == "c").unwrap();
        let pos_d = order.iter().position(|x| x == "d").unwrap();
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
        // 字典序: b < c
        assert!(pos_b < pos_c);
    }

    /// 环 a → b → a:报 Cyclic,错误信息含环上节点 label。
    #[test]
    fn cycle_returns_cyclic_error() {
        let mut g = DiGraph::new();
        g.add_node("a").unwrap();
        g.add_node("b").unwrap();
        g.add_edge("a", "b").unwrap();
        g.add_edge("b", "a").unwrap();
        let err = g.topo_sort().unwrap_err();
        match err {
            PipelineError::Cyclic(s) => assert!(s == "a" || s == "b"),
            other => panic!("expected Cyclic, got {other:?}"),
        }
    }

    /// 自环 a → a:也算 Cyclic。
    #[test]
    fn self_loop_is_cyclic() {
        let mut g = DiGraph::new();
        g.add_node("a").unwrap();
        g.add_edge("a", "a").unwrap();
        assert!(matches!(
            g.topo_sort().unwrap_err(),
            PipelineError::Cyclic(_)
        ));
    }

    /// 重复节点名报 DuplicateNode。
    #[test]
    fn duplicate_node_label_rejected() {
        let mut g = DiGraph::new();
        g.add_node("a").unwrap();
        let err = g.add_node("a").unwrap_err();
        assert!(matches!(err, PipelineError::DuplicateNode(_)));
    }

    /// 边引用不存在的节点报 UnknownNode。
    #[test]
    fn edge_to_unknown_node_rejected() {
        let mut g = DiGraph::new();
        g.add_node("a").unwrap();
        let err = g.add_edge("a", "ghost").unwrap_err();
        assert!(matches!(err, PipelineError::UnknownNode(_)));
    }

    /// `successors_of` 列出下游节点 label。
    #[test]
    fn successors_of_returns_downstream() {
        let mut g = DiGraph::new();
        for n in ["a", "b", "c"] {
            g.add_node(n).unwrap();
        }
        g.add_edge("a", "b").unwrap();
        g.add_edge("a", "c").unwrap();
        let mut succs = g.successors("a");
        succs.sort();
        assert_eq!(succs, vec!["b".to_string(), "c".to_string()]);
        assert!(g.successors("b").is_empty());
        assert!(g.successors("ghost").is_empty());
    }

    /// `node_count` / `edge_count` / `node_labels` 正确。
    #[test]
    fn accessors_match_construction() {
        let mut g = DiGraph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        for n in ["x", "y"] {
            g.add_node(n).unwrap();
        }
        g.add_edge("x", "y").unwrap();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.node_labels(), vec!["x", "y"]);
    }
}
