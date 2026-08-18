//! `reflect-pipeline` — DAG 驱动的多阶段流水线运行时。
//!
//! v1.1.0 Phase 5 落地。4 阶段预设(planner → prd → executor → verifier),把 `reflect-task::TeamNodeRunner`
//! 的 `SubAgentFactory::spawn` 能力串成可声明的 DAG。
//!
//! ## 设计动机
//!
//! 既有 `reflect-discussion::DiscussionOrchestrator` 的状态机(顺序 / 并发 +
//! 共识)与 DAG 引擎语义不同:discussion 是 round-based 多 agent 对话,
//! pipeline 是任意拓扑的阶段执行。把 DAG 引擎放在独立 crate,避免
//! `reflect-discussion` 变成 god-crate,同时让 Phase 4 的 coordinator
//! worker 模式能复用 `TeamNodeRunner` 模板(无需动 `reflect-discussion`)。
//!
//! ## 模块组织
//!
//! - [`error`] — `PipelineError` 统一错误类型。
//! - [`graph`] — 极简 `DiGraph` 实现 + 拓扑排序 + 循环检测。
//! - [`runner`] — `NodeRunner` trait + `NodeContext` + `NodeOutcome` + `NodeStatus`。
//! - [`template`] — `{{input.X}}` / `{{nodes.X.outputs.Y}}` 字符串渲染。
//! - [`pipeline`] — `Pipeline` 结构 + `Pipeline::from_toml` + `Pipeline::run` + `PipelineReport`。
//! - [`nodes`] — `TeamNodeRunner`(把 TeamFile 喂给 SubAgentFactory)+ 4 阶段预设。
//!
//! ## 设计原则
//!
//! - **DAG 自实现,不引入 petgraph**:Phase 5 plan 评估时优先考虑 petgraph,
//!   但本 crate 仅需 `add_node` / `add_edge` / `topo_sort` / `cycle_detect` 四件套,
//!   自实现 ~120 行足够,且零外部依赖(`Cargo.lock` 不会多一个 crate)。
//! - **失败策略由 `Pipeline.failure_policy` 决定**:`Abort`(任一节点 Failed 即终止)
//!   / `ContinueCollect`(跑完所有可执行的节点,失败的留标记)。
//! - **`TeamNodeRunner` 走 `SubAgentFactory::spawn` + drain**:与
//!   `reflect-subagent` 的 `SpawnedChild::collect_result_with_usage` 同源,
//!   不重复实现 LLM 集成层。
//! - **模板渲染自实现**:模板语法极简(`{{var}}` 与 `{{var.field}}`),不引入
//!   `minijinja`(已存在但属 prompt 模块)或 `handlebars`,避免反射依赖。

pub mod dag;
pub mod error;
pub mod graph;
pub mod nodes;
pub mod pipeline;
pub mod runner;
pub mod team_preset;
pub mod template;

pub use team_preset::{
    TEAM_PIPELINE_TOML, TeamPipelineExt, run_team_pipeline, run_team_pipeline_with_team,
};

pub use dag::levels_from_topo;
pub use error::PipelineError;
pub use graph::DiGraph;
pub use nodes::{
    PRESET_EXECUTOR_NAME, PRESET_PLANNER_NAME, PRESET_PRD_NAME, PRESET_VERIFIER_NAME,
    TeamNodeRunner, executor_node,
    human_gate::{HumanGateParams, HumanGateRunner},
    join::JoinNodeRunner,
    loop_control::{LoopControlRunner, LoopParams},
    planner_node, prd_node, verifier_node,
};
pub use pipeline::{FailurePolicy, NodeReport, Pipeline, PipelineContext, PipelineReport};
pub use runner::{NodeContext, NodeOutcome, NodeRunner, NodeStatus};
