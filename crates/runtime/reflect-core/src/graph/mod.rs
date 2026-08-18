//! `StateGraph` — 4 节点 agent 循环驱动器(M2/M3)。
//!
//! 走 `PreLoop → ModelCall → (ToolExec → PreLoop)* → CheckStop → (PreLoop | end)`。
//! `ToolExec` 回到 `PreLoop` 让模型基于工具结果继续多步推理;`CheckStop` 只在
//! 模型自然停止(无工具调用)时到达,Stop hook 可在此否决以强制续作。
//! M3 经 `nodes::check_stop` 与 `ToolExecutionQueue` 接入 hooks。

pub mod nodes;
pub mod state;

pub use state::AgentState;

use crate::submission_loop::NodeContext;

/// v0 agent 循环的四个节点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphNode {
    /// `pre_loop` — microcompact + reminder 注入 + skills 激活。
    PreLoop,
    /// `model_call` — LLM 调用 + prompt caching 注入。
    ModelCall,
    /// `tool_exec` — 经 `ToolExecutionQueue` 处理 tool_use blocks。
    ToolExec,
    /// `check_stop` — Stop hook + max-iteration 安全阀。
    CheckStop,
}

impl GraphNode {
    /// 默认转移:pre_loop → model_call → tool_exec → pre_loop(循环),
    /// 直到模型停止发出工具调用,然后 model_call → check_stop。
    /// `ToolExec → PreLoop` 让工具结果回灌模型继续多步推理(M2 多步回退修复);
    /// `CheckStop` 仅在模型自然停步时到达。
    pub fn next(self, has_tool_calls: bool) -> Option<GraphNode> {
        match (self, has_tool_calls) {
            (GraphNode::PreLoop, _) => Some(GraphNode::ModelCall),
            (GraphNode::ModelCall, true) => Some(GraphNode::ToolExec),
            (GraphNode::ModelCall, false) => Some(GraphNode::CheckStop),
            (GraphNode::ToolExec, _) => Some(GraphNode::PreLoop),
            (GraphNode::CheckStop, true) => Some(GraphNode::PreLoop),
            (GraphNode::CheckStop, false) => None, // turn complete
        }
    }
}

pub struct StateGraph {
    pub state: AgentState,
    pub ctx: NodeContext,
}

impl StateGraph {
    pub fn new(state: AgentState, ctx: NodeContext) -> Self {
        Self { state, ctx }
    }

    /// 走 4 节点循环。返回最终的 `AgentState`。
    /// 仅当循环在 `CheckStop` 终止(turn 自然结束)时,把
    /// `state.completed_normally` 置 `true`;`model_call` 因错误 / 取消
    /// 终止时不算。
    pub async fn run(mut self) -> AgentState {
        let mut node = GraphNode::PreLoop;
        loop {
            let next = match node {
                GraphNode::PreLoop => nodes::pre_loop(&mut self.state, &self.ctx).await,
                GraphNode::ModelCall => nodes::model_call(&mut self.state, &self.ctx).await,
                GraphNode::ToolExec => nodes::tool_exec(&mut self.state, &self.ctx).await,
                GraphNode::CheckStop => nodes::check_stop(&mut self.state, &self.ctx).await,
            };
            let Some(next) = next else {
                // 自然终止来自 check_stop 返回 None(= allow)。若
                // model_call / tool_exec 返回 None,turn 为异常结束,
                // 不标记 completed_normally。
                if matches!(node, GraphNode::CheckStop) {
                    self.state.completed_normally = true;
                }
                break;
            };
            node = next;
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_loop_always_goes_to_model_call() {
        assert_eq!(GraphNode::PreLoop.next(false), Some(GraphNode::ModelCall));
        assert_eq!(GraphNode::PreLoop.next(true), Some(GraphNode::ModelCall));
    }

    #[test]
    fn model_call_branches_on_tool_use() {
        assert_eq!(GraphNode::ModelCall.next(false), Some(GraphNode::CheckStop));
        assert_eq!(GraphNode::ModelCall.next(true), Some(GraphNode::ToolExec));
    }

    #[test]
    fn tool_exec_loops_back_to_pre_loop() {
        // 工具执行后回到 PreLoop → ModelCall,让模型基于工具结果继续推理。
        assert_eq!(GraphNode::ToolExec.next(false), Some(GraphNode::PreLoop));
        assert_eq!(GraphNode::ToolExec.next(true), Some(GraphNode::PreLoop));
    }

    #[test]
    fn check_stop_loops_or_terminates() {
        assert_eq!(GraphNode::CheckStop.next(true), Some(GraphNode::PreLoop));
        assert_eq!(GraphNode::CheckStop.next(false), None);
    }

    #[test]
    fn full_walk_terminates_after_no_tool_calls() {
        let mut node = GraphNode::PreLoop;
        let mut path = vec![node];
        while let Some(next) = node.next(false) {
            path.push(next);
            node = next;
        }
        assert_eq!(
            path,
            vec![
                GraphNode::PreLoop,
                GraphNode::ModelCall,
                GraphNode::CheckStop
            ]
        );
    }
}
