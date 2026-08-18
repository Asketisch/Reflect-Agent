//! `join` —— Join 节点 runner(P2 `pipeline-join`)。
//!
//! 等待所有直接上游节点完成,合并 outputs 供下游消费。

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::PipelineError;
use crate::runner::{NodeContext, NodeOutcome, NodeRunner, NodeStatus};

/// Join 节点 —— 聚合上游 outputs,不 spawn subagent。
pub struct JoinNodeRunner {
    pub name: String,
}

impl JoinNodeRunner {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl NodeRunner for JoinNodeRunner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        if ctx.cancel.is_cancelled() {
            return Ok(NodeOutcome::skipped());
        }

        let mut merged: HashMap<String, Value> = HashMap::new();
        let mut upstream_names: Vec<String> = ctx.inputs.keys().cloned().collect();
        upstream_names.sort();

        for (label, outputs) in &ctx.inputs {
            if outputs.is_null() {
                return Ok(NodeOutcome {
                    status: NodeStatus::Failed(format!(
                        "join '{name}': upstream '{label}' has no outputs",
                        name = self.name
                    )),
                    outputs: Value::Null,
                    error: None,
                });
            }
            merged.insert(label.clone(), outputs.clone());
        }

        Ok(NodeOutcome::success(json!({
            "joined": merged,
            "upstream": upstream_names,
            "count": merged.len(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use reflect_subagent::SubAgentFactory;
    use reflect_task::TaskManager;
    use reflect_task::{InMemoryTaskStore, InMemoryTeamStore};
    use tokio_util::sync::CancellationToken;

    fn ctx(inputs: HashMap<String, Value>) -> NodeContext {
        NodeContext {
            name: "join".into(),
            inputs,
            cancel: CancellationToken::new(),
            factory: Arc::new(dummy_factory()),
            manager: Arc::new(TaskManager::new(
                Arc::new(InMemoryTaskStore::new()),
                Arc::new(InMemoryTeamStore::new()),
            )),
            topic: "t".into(),
            iteration: 1,
        }
    }

    fn dummy_factory() -> SubAgentFactory {
        use reflect_llm::ModelRegistry;
        use reflect_protocol::ThreadId;
        use reflect_tools::ToolRegistry;
        SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            Arc::new(ModelRegistry::new()),
            None, // child_registry: will be set by caller if subagent_providers configured
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        )
    }

    #[tokio::test]
    async fn join_merges_upstream_outputs() {
        let mut inputs = HashMap::new();
        inputs.insert("a".into(), json!({"result": "A"}));
        inputs.insert("b".into(), json!({"result": "B"}));
        let runner = JoinNodeRunner::new("join");
        let out = runner.run(&ctx(inputs)).await.unwrap();
        assert!(out.status.is_success());
        assert_eq!(out.outputs["count"], 2);
        assert_eq!(out.outputs["joined"]["a"]["result"], "A");
    }

    #[tokio::test]
    async fn join_fails_when_upstream_null() {
        let mut inputs = HashMap::new();
        inputs.insert("bad".into(), Value::Null);
        let runner = JoinNodeRunner::new("join");
        let out = runner.run(&ctx(inputs)).await.unwrap();
        assert!(out.status.is_failure());
    }
}
