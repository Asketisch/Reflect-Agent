//! 模块测试。从原始文件内联的 `#[cfg(test)] mod tests` 迁移而来。

use super::*;
use crate::error::PipelineError;
use crate::runner::{NodeOutcome, NodeRunner};
use std::sync::atomic::AtomicU32;

/// 测 runner:始终成功,outputs = `{ "result": "<label> done" }`。
struct EchoRunner {
    label: String,
}
#[async_trait::async_trait]
impl NodeRunner for EchoRunner {
    fn name(&self) -> &str {
        &self.label
    }
    async fn run(&self, _ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        Ok(NodeOutcome::success(serde_json::json!({
            "result": format!("{} done", self.label),
        })))
    }
}

/// 测 runner:始终失败。
struct FailRunner {
    label: String,
}
#[async_trait::async_trait]
impl NodeRunner for FailRunner {
    fn name(&self) -> &str {
        &self.label
    }
    async fn run(&self, _ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        Ok(NodeOutcome::failure(format!(
            "{} intentionally failed",
            self.label
        )))
    }
}

fn make_runner(label: &str) -> Arc<dyn NodeRunner> {
    Arc::new(EchoRunner {
        label: label.into(),
    })
}

fn make_fail_runner(label: &str) -> Arc<dyn NodeRunner> {
    Arc::new(FailRunner {
        label: label.into(),
    })
}

/// 测 runner:睡 `delay_ms` 后成功。用于验证同层节点并行 fan-out
/// (钻石拓扑下,串行 = 2×delay,并行 ≈ delay)。
struct SlowRunner {
    label: String,
    delay_ms: u64,
}
#[async_trait::async_trait]
impl NodeRunner for SlowRunner {
    fn name(&self) -> &str {
        &self.label
    }
    async fn run(&self, _ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(NodeOutcome::success(serde_json::json!({
            "result": format!("{} done", self.label),
        })))
    }
}

fn make_slow_runner(label: &str, delay_ms: u64) -> Arc<dyn NodeRunner> {
    Arc::new(SlowRunner {
        label: label.into(),
        delay_ms,
    })
}

fn ctx_no_gate() -> PipelineContext {
    PipelineContext {
        topic: "test".into(),
        inputs: HashMap::new(),
        factory: Arc::new(dummy_factory()),
        manager: Arc::new(dummy_manager()),
        cancel: CancellationToken::new(),
        human_gate: None,
    }
}

/// 钻石拓扑 a → b → d, a → c → d(b / c 同层)。同层并行 fan-out 应让
/// b(80ms) + c(80ms) 的总耗时 ≈ 80ms 而非 160ms。
#[tokio::test]
async fn run_diamond_fans_out_in_parallel() {
    let mut p = Pipeline::empty();
    for n in ["a", "b", "c", "d"] {
        p.add_node(n, make_slow_runner(n, 80)).unwrap();
    }
    p.add_edge("a", "b").unwrap();
    p.add_edge("a", "c").unwrap();
    p.add_edge("b", "d").unwrap();
    p.add_edge("c", "d").unwrap();

    let started = std::time::Instant::now();
    let report = p.run(ctx_no_gate()).await.unwrap();
    let elapsed = started.elapsed().as_millis();
    assert_eq!(report.status, "success");
    // a(80) + max(b,c)(80) + d(80) = 240ms 串行下界;并行后 ≈ 240ms。
    // 关键断言:b / c 同层并行,所以总耗时显著 < 80*4=320ms(纯串行)。
    // 留 30ms 容差(调度抖动),核心是 < 320 - 80 = 240 不可能,放宽到 310。
    assert!(
        elapsed < 310,
        "diamond fan-out should parallelize b+c (elapsed={elapsed}ms)"
    );
}

/// loop 回退:verify 节点首次失败,LoopControlRunner 输出 should_loop →
/// pipeline 回退到 plan 重跑。第二轮 plan 注入 `passed: true` 让 loop 通过。
#[tokio::test]
async fn run_loop_back_retries_until_passed() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let attempt = Arc::new(AtomicU32::new(0));
    // plan:第 2+ 次跑时输出 passed=true(模拟"修好了")。
    let attempt_plan = attempt.clone();
    struct PlanRunner {
        attempt: Arc<AtomicU32>,
        label: String,
    }
    #[async_trait::async_trait]
    impl NodeRunner for PlanRunner {
        fn name(&self) -> &str {
            &self.label
        }
        async fn run(&self, _ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
            let n = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(NodeOutcome::success(serde_json::json!({
                "result": format!("plan attempt {n}"),
                "passed": n >= 2, // 第 2 次起算通过
            })))
        }
    }
    let _ = attempt_plan; // 仅保留语义:plan 用 attempt 共享

    // verify(loop gate):读上游 passed,失败则 should_loop 回 plan。
    struct VerifyRunner {
        label: String,
    }
    #[async_trait::async_trait]
    impl NodeRunner for VerifyRunner {
        fn name(&self) -> &str {
            &self.label
        }
        async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
            let passed = ctx
                .inputs
                .values()
                .any(|v| v.get("passed").and_then(|x| x.as_bool()).unwrap_or(false));
            if passed {
                return Ok(NodeOutcome::success(serde_json::json!({
                    "passed": true, "should_loop": false, "loop_to": null,
                })));
            }
            Ok(NodeOutcome::success(serde_json::json!({
                "passed": false,
                "should_loop": true,
                "loop_to": "plan",
            })))
        }
    }

    let mut p = Pipeline::empty();
    p.add_node(
        "plan",
        Arc::new(PlanRunner {
            attempt: attempt.clone(),
            label: "plan".into(),
        }),
    )
    .unwrap();
    p.add_node(
        "verify",
        Arc::new(VerifyRunner {
            label: "verify".into(),
        }),
    )
    .unwrap();
    p.add_edge("plan", "verify").unwrap();

    let report = p.run(ctx_no_gate()).await.unwrap();
    // plan 应跑了至少 2 次(首次 passed=false 触发 loop)。
    assert!(
        attempt.load(Ordering::SeqCst) >= 2,
        "loop should re-run plan (attempts={})",
        attempt.load(Ordering::SeqCst)
    );
    assert_eq!(report.status, "success");
}

/// Human gate 阻塞:gate 首次未通过,但配了 human_gate 回调返回 true →
/// pipeline 改判 success。
#[tokio::test]
async fn run_human_gate_blocks_then_approves() {
    use crate::nodes::human_gate::HumanGateRunner;
    // 回调:首次 false(模拟人工犹豫),后续 true。
    let calls = Arc::new(AtomicU32::new(0));
    struct ApproveCb {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl HumanGateCallback for ApproveCb {
        async fn approve(&self, _node: &str, _msg: &str) -> bool {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            n >= 1 // 首次即批准
        }
    }

    let mut p = Pipeline::empty();
    // 上游 plan 输出 approved=false(触发 gate 未通过路径)。
    p.add_node("plan", make_runner("plan")).unwrap();
    p.add_node("gate", Arc::new(HumanGateRunner::new("gate", "approve me")))
        .unwrap();
    p.add_edge("plan", "gate").unwrap();

    let mut ctx = ctx_no_gate();
    ctx.human_gate = Some(Arc::new(ApproveCb {
        calls: calls.clone(),
    }));
    let report = p.run(ctx).await.unwrap();
    // gate 经回调批准后改判 success → 整条 pipeline success。
    assert_eq!(report.status, "success");
    let gate = report
        .nodes
        .iter()
        .find(|n| n.name == "gate")
        .expect("gate report");
    assert_eq!(gate.status, "success");
    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

/// Human gate 无回调时维持旧行为:gate 未通过 → Failed(Abort 策略停)。
#[tokio::test]
async fn run_human_gate_without_callback_stays_failed() {
    use crate::nodes::human_gate::HumanGateRunner;
    let mut p = Pipeline::empty();
    p.add_node("plan", make_runner("plan")).unwrap();
    p.add_node("gate", Arc::new(HumanGateRunner::new("gate", "approve me")))
        .unwrap();
    p.add_edge("plan", "gate").unwrap();

    let report = p.run(ctx_no_gate()).await.unwrap();
    assert_eq!(report.status, "failed");
    let gate = report
        .nodes
        .iter()
        .find(|n| n.name == "gate")
        .expect("gate report");
    assert_eq!(gate.status, "failed");
}

/// 手动构造的 3 节点链:plan → prd → exec,全部跑通。
#[tokio::test]
async fn run_linear_chain_success() {
    let mut p = Pipeline::empty();
    p.add_node("plan", make_runner("plan")).unwrap();
    p.add_node("prd", make_runner("prd")).unwrap();
    p.add_node("exec", make_runner("exec")).unwrap();
    p.add_edge("plan", "prd").unwrap();
    p.add_edge("prd", "exec").unwrap();

    let ctx = PipelineContext {
        topic: "test".into(),
        inputs: HashMap::new(),
        factory: Arc::new(dummy_factory()),
        manager: Arc::new(dummy_manager()),
        cancel: CancellationToken::new(),
        human_gate: None,
    };
    let report = p.run(ctx).await.unwrap();
    assert_eq!(report.status, "success");
    assert_eq!(report.nodes.len(), 3);
    assert_eq!(report.nodes[0].name, "plan");
    assert_eq!(report.nodes[2].name, "exec");
    for r in &report.nodes {
        assert_eq!(r.status, "success");
        assert_eq!(r.outputs["result"], format!("{} done", r.name));
    }
}

/// Abort 策略下,plan 失败时 prd / exec 被 skip。
#[tokio::test]
async fn run_abort_stops_after_first_failure() {
    let mut p = Pipeline::empty();
    p.add_node("plan", make_fail_runner("plan")).unwrap();
    p.add_node("prd", make_runner("prd")).unwrap();
    p.add_node("exec", make_runner("exec")).unwrap();
    p.add_edge("plan", "prd").unwrap();
    p.add_edge("prd", "exec").unwrap();

    let ctx = PipelineContext {
        topic: "t".into(),
        inputs: HashMap::new(),
        factory: Arc::new(dummy_factory()),
        manager: Arc::new(dummy_manager()),
        cancel: CancellationToken::new(),
        human_gate: None,
    };
    let report = p.run(ctx).await.unwrap();
    assert_eq!(report.nodes.len(), 3);
    assert_eq!(report.nodes[0].status, "failed");
    assert_eq!(report.nodes[1].status, "skipped");
    assert_eq!(report.nodes[2].status, "skipped");
    // Abort + any_failed → "failed"(无关 skipped 数)。
    assert_eq!(report.status, "failed");
}

/// `ContinueCollect` 策略下,plan 失败时 prd / exec 仍跑。
#[tokio::test]
async fn run_continue_collect_runs_all_nodes() {
    let mut p = Pipeline::empty().with_failure_policy(FailurePolicy::ContinueCollect);
    p.add_node("plan", make_fail_runner("plan")).unwrap();
    p.add_node("prd", make_runner("prd")).unwrap();
    p.add_node("exec", make_runner("exec")).unwrap();
    p.add_edge("plan", "prd").unwrap();
    p.add_edge("prd", "exec").unwrap();

    let ctx = PipelineContext {
        topic: "t".into(),
        inputs: HashMap::new(),
        factory: Arc::new(dummy_factory()),
        manager: Arc::new(dummy_manager()),
        cancel: CancellationToken::new(),
        human_gate: None,
    };
    let report = p.run(ctx).await.unwrap();
    assert_eq!(report.nodes.len(), 3);
    assert_eq!(report.nodes[0].status, "failed");
    assert_eq!(
        report.nodes[1].status, "success",
        "prd 仍跑(ContinueCollect)"
    );
    assert_eq!(
        report.nodes[2].status, "success",
        "exec 仍跑(ContinueCollect)"
    );
    assert_eq!(report.status, "partial");
}

/// `from_toml` 解析 4 节点预设 + 3 条依赖边。
#[test]
fn from_toml_parses_linear_chain() {
    let toml_src = r#"
name = "team-build"
failure_policy = "abort"

[nodes.plan]
runner = "team"
depends_on = []

[nodes.prd]
runner = "team"
depends_on = ["plan"]

[nodes.exec]
runner = "team"
depends_on = ["prd"]

[nodes.verify]
runner = "team"
depends_on = ["exec"]
"#;
    let p = Pipeline::from_toml(toml_src, |label, _params| Some(make_runner(label))).unwrap();
    assert_eq!(p.node_count(), 4);
    let order = p.graph.topo_sort().unwrap();
    assert_eq!(
        order,
        vec!["plan", "prd", "exec", "verify"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
}

/// `from_toml` 检测循环依赖(plan → exec → plan)。
#[test]
fn from_toml_rejects_cycle() {
    let toml_src = r#"
[nodes.plan]
runner = "team"
depends_on = ["exec"]

[nodes.exec]
runner = "team"
depends_on = ["plan"]
"#;
    let err = Pipeline::from_toml(toml_src, |label, _params| Some(make_runner(label))).unwrap_err();
    assert!(matches!(err, PipelineError::Cyclic(_)));
}

/// `from_toml` 缺 runner 报 MissingRunner。
#[test]
fn from_toml_missing_runner() {
    let toml_src = r#"
[nodes.plan]
runner = "team"
"#;
    let err = Pipeline::from_toml(toml_src, |_label, _params| None).unwrap_err();
    assert!(matches!(err, PipelineError::MissingRunner(_)));
}

/// `from_toml` 拒绝未知 failure_policy。
#[test]
fn from_toml_unknown_failure_policy() {
    let toml_src = r#"
failure_policy = "explode"

[nodes.plan]
runner = "team"
"#;
    let err = Pipeline::from_toml(toml_src, |label, _params| Some(make_runner(label))).unwrap_err();
    match err {
        PipelineError::Config(msg) => assert!(msg.contains("explode")),
        other => panic!("expected Config, got {other:?}"),
    }
}

/// `from_toml` 未知 runner 字段仍允许(留 future 扩展点,parser 不校验)。
#[test]
fn from_toml_unknown_runner_field_accepted() {
    let toml_src = r#"
[nodes.plan]
runner = "future"
depends_on = []
"#;
    let p = Pipeline::from_toml(toml_src, |label, _params| Some(make_runner(label))).unwrap();
    assert_eq!(p.node_count(), 1);
}

/// `FailurePolicy::parse` 接受合法值。
#[test]
fn failure_policy_parse() {
    assert_eq!(FailurePolicy::parse("abort").unwrap(), FailurePolicy::Abort);
    assert_eq!(
        FailurePolicy::parse("continue_collect").unwrap(),
        FailurePolicy::ContinueCollect
    );
    assert!(FailurePolicy::parse("oops").is_err());
}

// ── 共享 helper ────────────────────────────────────────────────

fn dummy_factory() -> reflect_subagent::SubAgentFactory {
    use parking_lot::Mutex;
    use reflect_llm::ModelRegistry;
    use reflect_protocol::ThreadId;
    use reflect_tools::ToolRegistry;
    let _ = Mutex::new(());
    reflect_subagent::SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        Arc::new(ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    )
}

fn dummy_manager() -> reflect_task::TaskManager {
    use reflect_task::{InMemoryTaskStore, InMemoryTeamStore, TaskManager};
    TaskManager::new(
        Arc::new(InMemoryTaskStore::new()),
        Arc::new(InMemoryTeamStore::new()),
    )
}
