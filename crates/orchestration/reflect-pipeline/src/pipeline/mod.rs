//! `Pipeline` — DAG 编排器。
//!
//! 持有 [`DiGraph`] + runner 注册表 + 失败策略,提供 `from_toml` 解析 +
//! `run` 串行执行入口。
//!
//! # 失败策略
//!
//! - [`FailurePolicy::Abort`] — 任一节点 Failed 即停,后续节点标 Skipped。
//! - [`FailurePolicy::ContinueCollect`] — 失败的节点留 Failed 标记,但所有
//!   可执行节点(上游全 Success 的)继续跑;最后报告里能看到每个节点的结局。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::PipelineError;
use crate::graph::DiGraph;
use crate::runner::{NodeContext, NodeOutcome, NodeRunner};

/// 单节点执行报告(出现在 [`PipelineReport::nodes`])。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeReport {
    /// 节点 label。
    pub name: String,
    /// 执行状态(Success / Skipped / Failed)。
    pub status: String,
    /// 节点 outputs(serde_json::Value 序列化为字符串,便于 TUI 渲染)。
    pub outputs: serde_json::Value,
    /// 错误信息(失败时)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 执行耗时(毫秒)。
    pub elapsed_ms: u64,
    /// 执行时刻(自 UNIX_EPOCH 起毫秒,便于排序与离线分析)。
    pub started_at_ms: u64,
}

/// 流水线整体执行报告。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineReport {
    /// 流水线主题。
    pub topic: String,
    /// 整体状态:`"success"`(所有节点 Success)/ `"partial"`(部分 Failed 或 Skipped)
    /// / `"failed"`(Abort 策略下首节点就 Failed)。
    pub status: String,
    /// 失败策略(便于事后审计)。
    pub failure_policy: String,
    /// 各节点报告(按拓扑序排列)。
    pub nodes: Vec<NodeReport>,
    /// 总耗时(毫秒)。
    pub total_elapsed_ms: u64,
}

/// 失败策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// 任一节点 Failed 即停;后续节点标 `Skipped`。
    #[default]
    Abort,
    /// 失败的节点留 Failed 标记,所有可执行节点继续跑。
    ContinueCollect,
}

impl FailurePolicy {
    /// 从字符串解析,大小写敏感。允许值:`"abort"` / `"continue_collect"`。
    pub fn parse(s: &str) -> Result<Self, PipelineError> {
        match s {
            "abort" => Ok(FailurePolicy::Abort),
            "continue_collect" => Ok(FailurePolicy::ContinueCollect),
            other => Err(PipelineError::Config(format!(
                "unknown failure_policy '{other}' (expected 'abort' or 'continue_collect')"
            ))),
        }
    }
}

/// `Pipeline::run` 的上下文。
#[derive(Clone)]
pub struct PipelineContext {
    /// 流水线主题(模板里 `{{topic}}` 引用)。
    pub topic: String,
    /// runner 自定义输入(模板里 `{{input.<key>}}` 引用)。
    pub inputs: HashMap<String, String>,
    /// 共享 subagent 工厂。
    pub factory: Arc<reflect_subagent::SubAgentFactory>,
    /// 任务管理器。
    pub manager: Arc<reflect_task::TaskManager>,
    /// 取消 token。
    pub cancel: CancellationToken,
    /// 人工审批回调(P2 `pipeline-human-gate`)。
    ///
    /// `HumanGateRunner` 未通过时,`Pipeline::run` 调它阻塞等待真实审批决定
    /// (TUI / CLI stdin / webhook)。返回 `true` = 通过,重跑 gate;`false` =
    /// 驳回,gate 维持 Failed。`None` 时 gate 维持原 stub 行为(读上游 approved)。
    pub human_gate: Option<Arc<dyn HumanGateCallback>>,
}

/// 人工审批回调 —— 让 pipeline 在 gate 未通过时阻塞等待真实审批。
///
/// 默认实现(`HumanGateCallback` 的 `()` 实现)立即返回 `false`(未通过),
/// 保持"无回调 = 不阻塞"的旧行为。TUI / CLI 注入真实实现(stdin 等待 y/n)。
#[async_trait::async_trait]
pub trait HumanGateCallback: Send + Sync {
    /// `node` = gate 节点名,`message` = gate 配置的提示文案。
    /// 返回 `true` 表示人工批准。
    async fn approve(&self, node: &str, message: &str) -> bool;
}

#[async_trait::async_trait]
impl HumanGateCallback for () {
    async fn approve(&self, _node: &str, _message: &str) -> bool {
        false
    }
}

impl std::fmt::Debug for PipelineContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineContext")
            .field("topic", &self.topic)
            .field("inputs", &self.inputs.keys().collect::<Vec<_>>())
            .field("human_gate", &self.human_gate.is_some())
            .finish()
    }
}

/// 流水线配置(TOML 解析目标)。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    /// 流水线名(标识用,不影响执行)。
    #[serde(default)]
    pub name: String,
    /// 失败策略,默认 abort。
    #[serde(default)]
    pub failure_policy: Option<String>,
    /// 节点定义:key = 节点 label,value = 节点定义。
    #[serde(default)]
    pub nodes: HashMap<String, NodeConfigDef>,
}

/// 单节点 TOML 定义。
#[derive(Debug, Clone, Deserialize)]
pub struct NodeConfigDef {
    /// runner 类型(`"team"` / `"shell"`,见 [`crate::nodes::default_runner_for`])。
    #[serde(default = "default_runner")]
    pub runner: String,
    /// runner 接受的参数(由 `NodeRunner::from_config` 解析)。
    /// 用 `Option` 让 TOML 缺省时为 `None`,`None` 时透传给 runner 走默认值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<toml::Value>,
    /// 依赖的上游节点 label 列表;空 = 根节点。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// v1.4 C2:节点级重试配置。`None` = 不重试(历史行为)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
}

/// v1.4 C2:节点级重试配置(与 FailurePolicy 正交 —— 重试耗尽才算
/// 节点真正失败,之后才进入失败策略)。
///
/// ```toml
/// [nodes.build.retry]
/// max_attempts = 3   # 总尝试次数(含首次);1 = 不重试
/// backoff_ms = 500   # 两次尝试之间的固定间隔
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// 总尝试次数(含首次)。`0` 视为 1。
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// 重试间隔毫秒。
    #[serde(default)]
    pub backoff_ms: u64,
}

fn default_max_attempts() -> u32 {
    1
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_ms: 0,
        }
    }
}

fn default_runner() -> String {
    "team".to_string()
}

/// 流水线 = DAG + runner 注册表 + 失败策略。
pub struct Pipeline {
    graph: DiGraph,
    runners: HashMap<String, Arc<dyn NodeRunner>>,
    /// v1.4 C2:节点级重试配置(label → 配置;未配置 = 不重试)。
    retries: HashMap<String, RetryConfig>,
    failure_policy: FailurePolicy,
    name: String,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("name", &self.name)
            .field("failure_policy", &self.failure_policy)
            .field("node_count", &self.graph.node_count())
            .field("edge_count", &self.graph.edge_count())
            .finish()
    }
}

impl Pipeline {
    /// 用空白 graph 与默认 failure_policy 构造空 pipeline(测试用)。
    pub fn empty() -> Self {
        Self {
            graph: DiGraph::new(),
            runners: HashMap::new(),
            retries: HashMap::new(),
            failure_policy: FailurePolicy::default(),
            name: String::new(),
        }
    }

    /// 添加节点 + 配套 runner。重复 label 报 `DuplicateNode`。
    pub fn add_node(
        &mut self,
        label: impl Into<String>,
        runner: Arc<dyn NodeRunner>,
    ) -> Result<(), PipelineError> {
        let label = label.into();
        self.graph.add_node(label.clone())?;
        self.runners.insert(label, runner);
        Ok(())
    }

    /// 添加依赖边 `from → to`(即 `to` 依赖 `from`)。
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<(), PipelineError> {
        self.graph.add_edge(from, to)
    }

    /// 设置失败策略。
    pub fn with_failure_policy(mut self, p: FailurePolicy) -> Self {
        self.failure_policy = p;
        self
    }

    /// v1.4 C2:设置节点级重试配置(手工构造 Pipeline 的路径;
    /// from_toml 在解析 `[nodes.X.retry]` 时内部调用)。
    pub fn set_retry(&mut self, label: impl Into<String>, retry: RetryConfig) {
        self.retries.insert(label.into(), retry);
    }

    /// 节点数。
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// 从 TOML 字符串解析 pipeline 配置 + 用 `runner_for` 闭包为每个节点
    /// 实例化 runner。
    ///
    /// 闭包签名(v1.4 起透传 runner 类型):
    /// `runner_for(label: &str, runner: &str, params: Option<&toml::Value>) -> Option<Arc<dyn NodeRunner>>`
    /// —— 找不到返回 `None`,Pipeline 报 `MissingRunner`。
    /// 推荐直接用 [`crate::nodes::default_runner_for`] 组合。
    pub fn from_toml<F>(s: &str, mut runner_for: F) -> Result<Self, PipelineError>
    where
        F: FnMut(&str, &str, Option<&toml::Value>) -> Option<Arc<dyn NodeRunner>>,
    {
        let cfg: PipelineConfig =
            toml::from_str(s).map_err(|e| PipelineError::Config(e.to_string()))?;
        let mut pipeline = Pipeline::empty();
        pipeline.name = cfg.name;
        // failure_policy 解析放在 runner_for 之前 —— 非法值优先暴露,
        // 不让 `MissingRunner` 抢报(配置级错误先于 wiring 级错误)。
        pipeline.failure_policy = match cfg.failure_policy.as_deref() {
            None => FailurePolicy::default(),
            Some(p) => FailurePolicy::parse(p)?,
        };
        // 先建所有节点,再建边 —— 保证 `add_edge` 时所有节点已存在。
        for (label, node_def) in &cfg.nodes {
            let runner = runner_for(label, &node_def.runner, node_def.params.as_ref())
                .ok_or_else(|| PipelineError::MissingRunner(label.clone()))?;
            pipeline.add_node(label.clone(), runner)?;
            if let Some(retry) = &node_def.retry {
                pipeline.retries.insert(label.clone(), retry.clone());
            }
        }
        for (label, node_def) in &cfg.nodes {
            for dep in &node_def.depends_on {
                pipeline.add_edge(dep, label)?;
            }
        }
        // 拓扑排序预检:循环依赖直接报(不让 `run` 时才暴露)。
        let _ = pipeline.graph.topo_sort()?;
        Ok(pipeline)
    }

    /// 执行整条 pipeline,返回 [`PipelineReport`]。
    ///
    /// v1.2.0 三项增强(P2 `pipeline-dag` / `pipeline-loop` / `pipeline-human-gate`):
    /// - **同层并行 fan-out**:按拓扑层级分组,同层节点用 `join_all` 并行执行
    ///   (线性链退化为逐层单节点,行为不变;钻石 / fan-out 拓扑自动并行)。
    /// - **Loop 回退**:节点 outcome 含 `should_loop: true` + `loop_to` 时,
    ///   把执行指针回退到 `loop_to` 目标并递增 `iteration`,直至 `passed` 或
    ///   达 `loop_max`(由 `LoopControlRunner` 决策)。
    /// - **Human Gate 阻塞**:`HumanGateRunner` 未通过且 ctx 配了 `human_gate`
    ///   回调时,调 `approve(node, message)` 阻塞等待真实审批;`true` 重跑 gate,
    ///   `false` 维持 Failed。无回调时维持原 stub 行为。
    pub async fn run(&self, ctx: PipelineContext) -> Result<PipelineReport, PipelineError> {
        let started = Instant::now();
        // 拓扑分层(同层并行);`from_toml` 已预检环,这里再校一次。
        let levels = self.graph.topo_levels()?;
        // 全拓扑序(把各层拍平),用于报告与失败传递检查。
        let order: Vec<String> = levels.iter().flatten().cloned().collect();
        let total_started_ms = system_time_ms();

        // 已完成节点 outputs(label → outputs),供下游模板引用。
        let node_outputs: std::sync::Arc<std::sync::Mutex<HashMap<String, serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        // 已失败节点集合(Abort 策略下决定是否跳过下游)。
        let failed: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        // 节点报告(label → 最新一次执行的报告;loop 重跑会覆盖)。
        let reports: std::sync::Arc<std::sync::Mutex<HashMap<String, NodeReport>>> =
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        // loop 跟踪:每个节点的当前迭代(从 1 起)。loop 回退时从 loop_to
        // 往后的节点迭代 +1。
        let iterations: std::sync::Arc<std::sync::Mutex<HashMap<String, u32>>> =
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        // 判定一个 label 是否处于任一 failed 节点的传递下游(Abort 跳过用)。
        // 走 BFS:从所有 failed 出发,沿 successor 边可达即 blocked。
        let blocked_from_failed = |failed_set: &std::collections::HashSet<String>, label: &str| {
            if failed_set.is_empty() {
                return false;
            }
            let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut queue: std::collections::VecDeque<String> =
                failed_set.iter().cloned().collect();
            while let Some(cur) = queue.pop_front() {
                if !visited.insert(cur.clone()) {
                    continue;
                }
                if cur == label {
                    return true;
                }
                for succ in self.graph.successors_of(&cur) {
                    if !visited.contains(&succ) {
                        queue.push_back(succ);
                    }
                }
            }
            false
        };

        // 单节点执行:取 runner → 构造 ctx → 跑 → 写报告 / outputs / failed。
        // 返回 outcome(供 loop / gate 判定)。
        let pipeline_ref = PipelineRef {
            runners: &self.runners,
            graph: &self.graph,
            retries: &self.retries,
        };
        // 共享状态句柄的显式类型(避免 async 闭包参数里 `Arc<_>` 推断失败)。
        type OutputsStore = std::sync::Mutex<HashMap<String, serde_json::Value>>;
        type FailedStore = std::sync::Mutex<std::collections::HashSet<String>>;
        type ReportsStore = std::sync::Mutex<HashMap<String, NodeReport>>;
        let exec_node = |label: String,
                         iteration: u32,
                         ctx_clone: PipelineContext,
                         node_outputs: std::sync::Arc<OutputsStore>,
                         failed: std::sync::Arc<FailedStore>,
                         reports: std::sync::Arc<ReportsStore>,
                         gate: Option<Arc<dyn HumanGateCallback>>| async move {
            let runner = pipeline_ref
                .runners
                .get(&label)
                .cloned()
                .ok_or_else(|| PipelineError::MissingRunner(label.clone()))?;

            // 构造上游 outputs:直接上游(predecessors)的 outputs。
            let preds = pipeline_ref.graph.predecessors(&label);
            let inputs_map = {
                let store = node_outputs.lock().unwrap();
                preds
                    .iter()
                    .map(|p| {
                        (
                            p.clone(),
                            store.get(p).cloned().unwrap_or(serde_json::Value::Null),
                        )
                    })
                    .collect::<HashMap<_, _>>()
            };

            let node_ctx = NodeContext {
                name: label.clone(),
                inputs: inputs_map,
                cancel: ctx_clone.cancel.clone(),
                factory: ctx_clone.factory.clone(),
                manager: ctx_clone.manager.clone(),
                topic: ctx_clone.topic.clone(),
                iteration,
            };

            debug!(node = %label, iteration, "pipeline: running node");
            let node_started = Instant::now();
            let started_at_ms = system_time_ms();
            // v1.4 C2:节点级重试 —— Failed 且还有剩余尝试时按 backoff_ms
            // 间隔重跑 runner。与 FailurePolicy 正交:重试耗尽才算节点真正
            // 失败,之后才进入 Abort / ContinueCollect 判定。取消令牌触发
            // 时立即停止重试。
            let retry_cfg = pipeline_ref.retries.get(&label);
            let max_attempts = retry_cfg.map(|r| r.max_attempts.max(1)).unwrap_or(1);
            let backoff_ms = retry_cfg.map(|r| r.backoff_ms).unwrap_or(0);
            let mut attempt: u32 = 1;
            let mut outcome = loop {
                let o = match runner.run(&node_ctx).await {
                    Ok(o) => o,
                    Err(e) => NodeOutcome::failure(e.to_string()),
                };
                let failed = o.is_failure();
                if !failed || attempt >= max_attempts || node_ctx.cancel.is_cancelled() {
                    break o;
                }
                warn!(
                    node = %label,
                    attempt,
                    max_attempts,
                    "pipeline: node failed, retrying"
                );
                if backoff_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                }
                attempt += 1;
            };

            // Human Gate 阻塞:Failed 且配了回调时,阻塞等审批,true 则改判 success。
            // runner 输出 `{"approved": false, "message": ...}` 是 gate 的契约。
            let is_gate_fail = matches!(&outcome.status, crate::runner::NodeStatus::Failed(_))
                && outcome
                    .outputs
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .map(|b| !b)
                    .unwrap_or(false);
            if is_gate_fail && let Some(cb) = gate {
                let message = outcome
                    .outputs
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("awaiting approval");
                let approved = cb.approve(&label, message).await;
                if approved {
                    outcome = NodeOutcome::success(serde_json::json!({
                        "approved": true,
                        "message": message,
                        "source": "human_gate_callback",
                    }));
                }
            }

            let elapsed_ms = node_started.elapsed().as_millis() as u64;
            let status_str = match &outcome.status {
                crate::runner::NodeStatus::Success => "success",
                crate::runner::NodeStatus::Skipped => "skipped",
                crate::runner::NodeStatus::Failed(_) => "failed",
            };
            info!(
                node = %label,
                status = status_str,
                elapsed_ms,
                "pipeline: node done"
            );

            let error_msg = match &outcome.status {
                crate::runner::NodeStatus::Failed(msg) => {
                    failed.lock().unwrap().insert(label.clone());
                    Some(msg.clone())
                }
                _ => None,
            };
            if outcome.status.is_success() {
                node_outputs
                    .lock()
                    .unwrap()
                    .insert(label.clone(), outcome.outputs.clone());
            }
            reports.lock().unwrap().insert(
                label.clone(),
                NodeReport {
                    name: label.clone(),
                    status: status_str.into(),
                    outputs: outcome.outputs.clone(),
                    error: error_msg,
                    elapsed_ms,
                    started_at_ms,
                },
            );
            Ok::<NodeOutcome, PipelineError>(outcome)
        };

        // 主执行循环:逐层并行,层内 join_all。loop 回退通过"重入目标层"实现。
        // 用层索引 `li` 推进;loop 时把 `li` 跳到 loop_to 所在层 -1(下一轮自然重跑)。
        let mut li = 0usize;
        // 上限保护:层数 × loop_max 不该爆炸,给个合理上限。
        let max_steps = levels.len().saturating_mul(32).max(levels.len()) + 32;
        let mut steps = 0usize;
        while li < levels.len() {
            steps += 1;
            if steps > max_steps {
                return Err(PipelineError::Config(format!(
                    "pipeline exceeded {max_steps} steps (possible loop runaway)"
                )));
            }
            if ctx.cancel.is_cancelled() {
                break;
            }

            let layer = levels[li].clone();
            // Abort 策略:本层节点若在 failed 的传递下游,标 skipped 不跑。
            let failed_snapshot = failed.lock().unwrap().clone();
            let mut runnables: Vec<String> = Vec::new();
            for label in &layer {
                let blocked = blocked_from_failed(&failed_snapshot, label);
                if blocked && self.failure_policy == FailurePolicy::Abort {
                    reports.lock().unwrap().insert(
                        label.clone(),
                        NodeReport {
                            name: label.clone(),
                            status: "skipped".into(),
                            outputs: serde_json::Value::Null,
                            error: Some("upstream failed (abort policy)".into()),
                            elapsed_ms: 0,
                            started_at_ms: total_started_ms,
                        },
                    );
                } else {
                    runnables.push(label.clone());
                }
            }

            // 同层并行 fan-out。
            let futs = runnables.iter().map(|label| {
                let it = iterations.lock().unwrap().get(label).copied().unwrap_or(1);
                exec_node(
                    label.clone(),
                    it,
                    ctx.clone(),
                    std::sync::Arc::clone(&node_outputs),
                    std::sync::Arc::clone(&failed),
                    std::sync::Arc::clone(&reports),
                    ctx.human_gate.clone(),
                )
            });
            let outcomes = futures::future::join_all(futs).await;

            // loop 回退检查:任一 outcome 带 should_loop + loop_to → 回退。
            let mut loop_back_to: Option<String> = None;
            for (label, outcome_res) in runnables.iter().zip(outcomes.iter()) {
                if let Ok(o) = outcome_res
                    && o.outputs
                        .get("should_loop")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    && let Some(target) = o.outputs.get("loop_to").and_then(|v| v.as_str())
                {
                    // 取第一个 loop 请求(同层多 loop 罕见,取首个)。
                    if loop_back_to.is_none() {
                        loop_back_to = Some(target.to_string());
                        // 标记本节点迭代 +1(loop_to 起的节点也 +1)。
                        iterations
                            .lock()
                            .unwrap()
                            .entry(label.clone())
                            .and_modify(|i| *i += 1)
                            .or_insert(2);
                    }
                }
            }

            if let Some(target) = loop_back_to {
                // 找 target 所在层,回退到它(包含)重跑。target 层之前的
                // 节点不重跑(已 success 的 outputs 保留)。
                let target_level = levels
                    .iter()
                    .position(|lvl| lvl.iter().any(|l| l == &target))
                    .ok_or_else(|| {
                        PipelineError::Config(format!(
                            "loop_to target '{target}' not found in pipeline"
                        ))
                    })?;
                // 从 target 到当前层之间所有节点的迭代 +1。
                for lvl in &levels[target_level..=li] {
                    for l in lvl {
                        iterations
                            .lock()
                            .unwrap()
                            .entry(l.clone())
                            .and_modify(|i| *i += 1)
                            .or_insert(2);
                    }
                }
                li = target_level;
                continue;
            }

            li += 1;
        }

        // 收尾:取消的未跑节点补 skipped 报告(保留全部拓扑序)。
        {
            let mut store = reports.lock().unwrap();
            for label in &order {
                if ctx.cancel.is_cancelled() && !store.contains_key(label) {
                    store.insert(
                        label.clone(),
                        NodeReport {
                            name: label.clone(),
                            status: "skipped".into(),
                            outputs: serde_json::Value::Null,
                            error: Some("pipeline cancelled".into()),
                            elapsed_ms: 0,
                            started_at_ms: total_started_ms,
                        },
                    );
                }
            }
        }

        // 拍平报告(按拓扑序)。
        let final_reports: Vec<NodeReport> = {
            let store = reports.lock().unwrap();
            order.iter().filter_map(|l| store.get(l).cloned()).collect()
        };

        // 整体状态(Abort+failed → failed;ContinueCollect+failed → partial;
        // 无 failed 有 skipped → partial;否则 success)。
        let any_failed = final_reports.iter().any(|r| r.status == "failed");
        let any_skipped = final_reports.iter().any(|r| r.status == "skipped");
        let overall = if any_failed {
            match self.failure_policy {
                FailurePolicy::Abort => "failed",
                FailurePolicy::ContinueCollect => "partial",
            }
        } else if any_skipped {
            "partial"
        } else {
            "success"
        };

        Ok(PipelineReport {
            topic: ctx.topic,
            status: overall.into(),
            failure_policy: match self.failure_policy {
                FailurePolicy::Abort => "abort",
                FailurePolicy::ContinueCollect => "continue_collect",
            }
            .into(),
            nodes: final_reports,
            total_elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// 闭包捕获用:把 `&Pipeline` 的字段引用打包,避免 `exec_node` 闭包借用 `self`
/// 与 `futures::join_all` 的生命周期冲突。
struct PipelineRef<'a> {
    runners: &'a HashMap<String, Arc<dyn NodeRunner>>,
    graph: &'a DiGraph,
    /// v1.4 C2:节点级重试配置(未配置 = 不重试)。
    retries: &'a HashMap<String, RetryConfig>,
}

fn system_time_ms() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
