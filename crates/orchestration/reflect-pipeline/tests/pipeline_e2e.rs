//! `reflect-pipeline` 端到端集成测试 —— mock 4 节点流水线,验证:
//!   1. TOML 配置解析 + 拓扑排序产出确定性顺序(plan → prd → exec → verify)。
//!   2. 节点 outputs 沿 `depends_on` 边传递给下游。
//!   3. `FailurePolicy::Abort` 在第一处失败后停止后续节点(transitively blocked)。
//!   4. `FailurePolicy::ContinueCollect` 把所有节点跑完,整体 status = "partial"。
//!
//! LLM 调用由 `MockSubAgentFactory` 替代 —— 每个节点返回一个常量
//! `serde_json::Value`,以 `elapsed_ms` 与确定性 `result` 字段为主。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use reflect_pipeline::{
    FailurePolicy, NodeContext, NodeOutcome, NodeRunner, Pipeline, PipelineContext, PipelineReport,
    runner::NodeName,
};
use reflect_protocol::ThreadId;
use reflect_subagent::SubAgentFactory;
use reflect_task::{InMemoryTaskStore, InMemoryTeamStore, TaskManager};
use tokio_util::sync::CancellationToken;

use reflect_llm::ModelRegistry;
use reflect_tools::ToolRegistry;

// ── 测试 helper ──────────────────────────────────────────────────────────

/// 用固定 JSON 输出的 mock runner —— 跑成功路径。
struct FixedRunner {
    name: String,
    payload: serde_json::Value,
}

#[async_trait]
impl NodeRunner for FixedRunner {
    fn name(&self) -> &str {
        &self.name
    }
    async fn run(
        &self,
        _ctx: &NodeContext,
    ) -> Result<NodeOutcome, reflect_pipeline::PipelineError> {
        Ok(NodeOutcome::success(self.payload.clone()))
    }
}

/// 跑失败路径的 mock runner —— 注入指定错误信息。
struct FailingRunner {
    name: String,
    err: String,
}

#[async_trait]
impl NodeRunner for FailingRunner {
    fn name(&self) -> &str {
        &self.name
    }
    async fn run(
        &self,
        _ctx: &NodeContext,
    ) -> Result<NodeOutcome, reflect_pipeline::PipelineError> {
        Ok(NodeOutcome::failure(self.err.clone()))
    }
}

/// 顶层 stub factory —— pipeline 不实际 spawn LLM,但 `PipelineContext`
/// 字段需要 `Arc<SubAgentFactory>`,构造一个空壳即可。
fn stub_factory(manager: Arc<TaskManager>) -> Arc<SubAgentFactory> {
    let registry = Arc::new(ModelRegistry::new());
    let parent_tools = Arc::new(ToolRegistry::default());
    Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        registry,
        None, // child_registry: will be set by caller if subagent_providers configured
        parent_tools,
        CancellationToken::new(),
        None,
    ))
    // 注:manager 在 e2e 中主要给 `inject_team` 路径使用;CLI 集成里 manager
    // 通过 `PipelineContext.manager` 传入,这里 stub factory 不实际用。
    .tap(|_| drop(manager))
}

/// 一个最简的 `tap` 工具:在 `Option` / `Result` 风格里 `let _ = x; x`。
trait Tap: Sized {
    fn tap<F: FnOnce(&Self)>(self, f: F) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}

fn stub_manager() -> Arc<TaskManager> {
    Arc::new(TaskManager::new(
        Arc::new(InMemoryTaskStore::default()),
        Arc::new(InMemoryTeamStore::default()),
    ))
}

fn ctx() -> PipelineContext {
    PipelineContext {
        topic: "build a CLI todo app".into(),
        inputs: HashMap::<NodeName, String>::new(),
        factory: stub_factory(stub_manager()),
        manager: stub_manager(),
        cancel: CancellationToken::new(),
        human_gate: None,
    }
}

const LINEAR_TOML: &str = r#"
[pipeline]
name = "linear-4"
failure_policy = "abort"

[nodes.plan]
team = "plan"
template = "Plan for: {{topic}}"
depends_on = []

[nodes.prd]
team = "prd"
template = "PRD from: {{nodes.plan.outputs.result}}"
depends_on = ["plan"]

[nodes.exec]
team = "exec"
template = "Exec from: {{nodes.prd.outputs.result}}"
depends_on = ["prd"]

[nodes.verify]
team = "verify"
template = "Verify: {{nodes.exec.outputs.result}}"
depends_on = ["exec"]
"#;

// ── e2e 测试 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn linear_chain_e2e_4_nodes_in_order() {
    let runners: Vec<Arc<dyn NodeRunner>> = vec![
        Arc::new(FixedRunner {
            name: "plan".into(),
            payload: serde_json::json!({"result": "PLAN"}),
        }),
        Arc::new(FixedRunner {
            name: "prd".into(),
            payload: serde_json::json!({"result": "PRD"}),
        }),
        Arc::new(FixedRunner {
            name: "exec".into(),
            payload: serde_json::json!({"result": "EXEC"}),
        }),
        Arc::new(FixedRunner {
            name: "verify".into(),
            payload: serde_json::json!({"result": "VERIFY"}),
        }),
    ];
    let pipeline = Pipeline::from_toml(LINEAR_TOML, |label, _params| {
        runners.iter().find(|r| r.name() == label).cloned()
    })
    .expect("parse");

    let report: PipelineReport = pipeline.run(ctx()).await.expect("run");

    assert_eq!(report.status, "success");
    assert_eq!(report.failure_policy, "abort");
    assert_eq!(report.nodes.len(), 4);
    let names: Vec<&str> = report.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["plan", "prd", "exec", "verify"]);
    for n in &report.nodes {
        assert_eq!(n.status, "success", "node {} should be success", n.name);
    }
}

#[tokio::test]
async fn abort_policy_stops_after_first_failure() {
    // 中间节点 exec 失败,verify 应当被 transitive blocked(skipped)。
    let runners: Vec<Arc<dyn NodeRunner>> = vec![
        Arc::new(FixedRunner {
            name: "plan".into(),
            payload: serde_json::json!({"result": "P"}),
        }),
        Arc::new(FixedRunner {
            name: "prd".into(),
            payload: serde_json::json!({"result": "PR"}),
        }),
        Arc::new(FailingRunner {
            name: "exec".into(),
            err: "compile error".into(),
        }),
        Arc::new(FixedRunner {
            name: "verify".into(),
            payload: serde_json::json!({"result": "V"}),
        }),
    ];
    let pipeline = Pipeline::from_toml(LINEAR_TOML, |label, _params| {
        runners.iter().find(|r| r.name() == label).cloned()
    })
    .expect("parse");

    let report = pipeline.run(ctx()).await.expect("run");

    assert_eq!(report.status, "failed");
    // verify 是 exec 的下游,Abort 下应被跳过。
    let verify = report.nodes.iter().find(|n| n.name == "verify").unwrap();
    assert_eq!(verify.status, "skipped");
    let exec = report.nodes.iter().find(|n| n.name == "exec").unwrap();
    assert_eq!(exec.status, "failed");
}

#[tokio::test]
async fn continue_collect_runs_all_nodes_after_failure() {
    let runners: Vec<Arc<dyn NodeRunner>> = vec![
        Arc::new(FixedRunner {
            name: "plan".into(),
            payload: serde_json::json!({"result": "P"}),
        }),
        Arc::new(FailingRunner {
            name: "prd".into(),
            err: "spec incomplete".into(),
        }),
        Arc::new(FixedRunner {
            name: "exec".into(),
            payload: serde_json::json!({"result": "E"}),
        }),
        Arc::new(FixedRunner {
            name: "verify".into(),
            payload: serde_json::json!({"result": "V"}),
        }),
    ];
    let pipeline = Pipeline::from_toml(LINEAR_TOML, |label, _params| {
        runners.iter().find(|r| r.name() == label).cloned()
    })
    .expect("parse")
    .with_failure_policy(FailurePolicy::ContinueCollect);

    let report = pipeline.run(ctx()).await.expect("run");

    // ContinueCollect 把所有节点都跑完(不跳过,只标记 failed);整体 status = partial。
    assert_eq!(report.status, "partial");
    assert_eq!(report.failure_policy, "continue_collect");
    let statuses: Vec<&str> = report.nodes.iter().map(|n| n.status.as_str()).collect();
    assert_eq!(
        statuses,
        vec!["success", "failed", "success", "success"],
        "ContinueCollect 应当跑完所有节点,failed 节点不阻塞后续"
    );
}

#[tokio::test]
async fn runner_records_duration_per_node() {
    // 每个节点 elapsed_ms >= 0 —— 简单 sanity check。
    let runners: Vec<Arc<dyn NodeRunner>> = vec![
        Arc::new(FixedRunner {
            name: "plan".into(),
            payload: serde_json::json!({"result": "P"}),
        }),
        Arc::new(FixedRunner {
            name: "prd".into(),
            payload: serde_json::json!({"result": "PR"}),
        }),
        Arc::new(FixedRunner {
            name: "exec".into(),
            payload: serde_json::json!({"result": "E"}),
        }),
        Arc::new(FixedRunner {
            name: "verify".into(),
            payload: serde_json::json!({"result": "V"}),
        }),
    ];
    let pipeline = Pipeline::from_toml(LINEAR_TOML, |label, _params| {
        runners.iter().find(|r| r.name() == label).cloned()
    })
    .expect("parse");

    let report = pipeline.run(ctx()).await.expect("run");
    let sum: u64 = report.nodes.iter().map(|n| n.elapsed_ms).sum();
    assert!(
        report.total_elapsed_ms >= sum,
        "total_elapsed_ms {} 应当 ≥ 各节点 elapsed_ms 之和 {}",
        report.total_elapsed_ms,
        sum
    );
}

#[test]
fn unknown_failure_policy_in_toml_errors_at_parse() {
    let bad = r#"
name = "x"
failure_policy = "panic_on_error"

[nodes.a]
team = "a"
template = "x"
depends_on = []
"#;
    let err = Pipeline::from_toml(bad, |_, _| None::<Arc<dyn NodeRunner>>).unwrap_err();
    assert_eq!(err.kind(), "config");
}

// ── 抑制未用变量警告 ────────────────────────────────────────────────────

#[allow(dead_code)]
fn _unused_mutex() -> Mutex<()> {
    Mutex::new(())
}
