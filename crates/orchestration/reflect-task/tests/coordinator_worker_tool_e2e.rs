//! coordinator worker 工具白名单 e2e:INTERNAL_WORKER_TOOLS 从 worker registry 排除。

use std::sync::Arc;

use reflect_protocol::ThreadId;
use reflect_subagent::SubAgentFactory;
use reflect_task::coordinator::{INTERNAL_WORKER_TOOLS, build_worker_tool_registry};
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

struct StubTool(&'static str);

#[async_trait::async_trait]
impl reflect_tools::Tool for StubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "stub"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _: reflect_tools::ToolContext,
        _: serde_json::Value,
    ) -> Result<reflect_protocol::ToolOutput, reflect_tools::ToolError> {
        unimplemented!()
    }
}

#[test]
fn worker_registry_excludes_internal_tools() {
    let parent = ToolRegistry::default();
    for n in INTERNAL_WORKER_TOOLS {
        parent.register(Arc::new(StubTool(n)));
    }
    parent.register(Arc::new(StubTool("TaskCreate")));
    parent.register(Arc::new(StubTool("Read")));

    let worker = build_worker_tool_registry(&parent);
    let names = worker.list();
    for forbidden in INTERNAL_WORKER_TOOLS {
        assert!(
            !names.contains(&forbidden.to_string()),
            "{forbidden} 应被排除"
        );
    }
    assert!(names.contains(&"TaskCreate".to_string()));
    assert!(names.contains(&"Read".to_string()));
}

#[tokio::test]
async fn factory_coordinator_mode_enables_worker_path() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    assert!(!factory.is_coordinator_mode());
    factory.set_coordinator_mode(true, Some("worker footer".into()));
    assert!(factory.is_coordinator_mode());
    assert_eq!(
        factory.coordinator_footer().as_deref(),
        Some("worker footer")
    );
}
