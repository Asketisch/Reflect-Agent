//! `NodeRunner` trait — pipeline 内单个节点的执行抽象。
//!
//! 设计原则:每节点接收 [`NodeContext`] 上下文,产出 [`NodeOutcome`] 结果。
//! `Pipeline::run` 负责编排(拓扑排序 + 失败策略),节点本身只关心"如何
//! 把上游输出 + 当前节点模板跑出输出"。
//!
//! # 内置 runner
//!
//! - [`crate::nodes::TeamNodeRunner`] — 把 `TeamFile` 的成员 spec 注入
//!   `SubAgentFactory`,用 `task_template` 渲染 prompt,spawn + drain。
//! - 4 阶段预设:`planner` / `prd` / `executor` / `verifier`(见 [`crate::nodes`])。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use reflect_subagent::SubAgentFactory;
use reflect_task::TaskManager;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 节点名 = pipeline 内的唯一标识(对应 `DiGraph` 的 label)。
pub type NodeName = String;

/// 节点执行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    /// 节点成功跑完,outputs 可被下游消费。
    Success,
    /// 节点跳过(由 `Pipeline::run` 在某些前置条件下跳过,例如 Abort 策略下
    /// 上游已 Failed 时)。
    Skipped,
    /// 节点执行失败,错误信息附在 `NodeOutcome::error`。
    Failed(String),
}

impl NodeStatus {
    /// 是否算"成功完成"(Success 走后续 + Skipped 视 caller 决定)。
    pub fn is_success(&self) -> bool {
        matches!(self, NodeStatus::Success)
    }

    /// 是否阻断流水线(`Abort` 策略下遇到即停)。
    pub fn is_failure(&self) -> bool {
        matches!(self, NodeStatus::Failed(_))
    }
}

/// 节点产出 — outputs + status + 可选 error。
///
/// `outputs` 是 `serde_json::Value`,由 `task_template` 渲染时或 spawn
/// drain 后解析得来。pipeline `run` 把每节点 outputs 存到 `NodeContext::inputs`,
/// 给下游 `{{nodes.X.outputs.Y}}` 引用。
#[derive(Debug, Clone)]
pub struct NodeOutcome {
    /// 节点状态。
    pub status: NodeStatus,
    /// 节点产出,推荐结构:`{ "result": "<text>" }`,但任意 JSON 都可。
    /// 模板里 `{{nodes.X.outputs.Y}}` 通过 `Y` 字段查找。
    pub outputs: Value,
    /// 错误信息(仅 `status == Failed` 时有值)。
    pub error: Option<String>,
}

impl NodeOutcome {
    /// 是否失败(v1.4 C2:重试循环判定用)。
    pub fn is_failure(&self) -> bool {
        matches!(self.status, NodeStatus::Failed(_))
    }

    /// 构造成功 outcome,outputs 默认为空对象。
    pub fn success(outputs: Value) -> Self {
        Self {
            status: NodeStatus::Success,
            outputs,
            error: None,
        }
    }

    /// 构造失败 outcome。
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            status: NodeStatus::Failed(error.into()),
            outputs: Value::Null,
            error: None,
        }
    }

    /// 构造跳过 outcome。
    pub fn skipped() -> Self {
        Self {
            status: NodeStatus::Skipped,
            outputs: Value::Null,
            error: None,
        }
    }
}

/// 节点执行上下文 —— 由 `Pipeline::run` 在调用 `NodeRunner::run` 前构造。
///
/// - `inputs` = 当前节点的直接上游节点的 outputs(以节点 label 为 key)。
///   模板渲染时 `{{nodes.<label>.outputs.<field>}` 通过本字段查找。
/// - `cancel` = 整个 pipeline 的取消 token;runner 应在长操作中 poll。
/// - `factory` / `manager` = 共享 LLM 子 agent 工厂与任务管理器。
/// - `topic` = 流水线主题(整条流水线统一)。
/// - `iteration` = 当前节点所在的循环迭代(从 1 起;无 loop 时恒为 1)。
///   `LoopControlRunner` 用它与 `loop_max` 比较决定是否回退。
#[derive(Clone)]
pub struct NodeContext {
    /// 当前节点名(同 DiGraph label)。
    pub name: NodeName,
    /// 上游节点 outputs(label → outputs Value)。
    pub inputs: HashMap<NodeName, Value>,
    /// 整条 pipeline 的取消 token。
    pub cancel: CancellationToken,
    /// 共享 subagent 工厂。`TeamNodeRunner` 通过 `factory.add_spec` +
    /// `factory.spawn` 调 LLM。
    pub factory: Arc<SubAgentFactory>,
    /// 任务管理器 —— runner 可用来 create / list / update task。
    pub manager: Arc<TaskManager>,
    /// 流水线主题(`reflect pipeline run --topic X` 的 `X`)。
    pub topic: String,
    /// 当前循环迭代(从 1 起;无 loop 回退时恒为 1)。`LoopControlRunner`
    /// 读它判断 `iteration < loop_max` 是否还能回退。
    pub iteration: u32,
}

impl std::fmt::Debug for NodeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeContext")
            .field("name", &self.name)
            .field("inputs", &self.inputs.keys().collect::<Vec<_>>())
            .field("topic", &self.topic)
            .field("iteration", &self.iteration)
            .finish()
    }
}

/// 单个节点的执行抽象。
///
/// `run` 接收 [`NodeContext`],返回 [`NodeOutcome`]。runner 实现负责:
/// 1. 用 `template::render(&self.task_template, ctx)` 渲染 prompt。
/// 2. 调 `ctx.factory.spawn(...)` 拿 `SpawnedChild`。
/// 3. `child.collect_result_with_usage()` drain → 解析为 `NodeOutcome::outputs`。
/// 4. 出错返 [`NodeOutcome::failure`]。
///
/// runner 自身**不**处理依赖检查 / 失败策略 —— 由 `Pipeline::run` 编排。
#[async_trait]
pub trait NodeRunner: Send + Sync {
    /// 节点名(返回 `self.name` 字段即可,Pipeline::run 也会校验)。
    fn name(&self) -> &str;

    /// 执行节点逻辑。
    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, crate::error::PipelineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_status_helpers() {
        assert!(NodeStatus::Success.is_success());
        assert!(!NodeStatus::Success.is_failure());
        assert!(!NodeStatus::Skipped.is_success());
        assert!(NodeStatus::Failed("x".into()).is_failure());
    }

    #[test]
    fn outcome_constructors_set_status() {
        let s = NodeOutcome::success(serde_json::json!({"k": 1}));
        assert_eq!(s.status, NodeStatus::Success);
        assert_eq!(s.outputs["k"], 1);

        let f = NodeOutcome::failure("oops");
        assert!(matches!(f.status, NodeStatus::Failed(_)));

        let sk = NodeOutcome::skipped();
        assert_eq!(sk.status, NodeStatus::Skipped);
    }
}
