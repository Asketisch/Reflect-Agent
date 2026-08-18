//! `human_gate` —— 人工审批节点(P2 `pipeline-human-gate`)。
//!
//! 检查上游 `approved` 字段或 `ctx.inputs` 中的 gate 信号;
//! 未通过时返回 Failed,供 Abort 策略终止流水线。

use async_trait::async_trait;
use serde_json::json;

use crate::error::PipelineError;
use crate::runner::{NodeContext, NodeOutcome, NodeRunner, NodeStatus};

/// Human Gate 节点参数。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct HumanGateParams {
    /// 审批消息(写入 outputs)。
    #[serde(default)]
    pub message: Option<String>,
}

/// 人工审批 gate —— stub 实现,读上游 `approved: true` 或
/// `inputs.gate_approved = "true"`。
pub struct HumanGateRunner {
    pub name: String,
    pub message: String,
}

impl HumanGateRunner {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }

    pub fn from_params(name: impl Into<String>, params: &HumanGateParams) -> Self {
        Self::new(
            name,
            params
                .message
                .clone()
                .unwrap_or_else(|| "awaiting human approval".into()),
        )
    }
}

#[async_trait]
impl NodeRunner for HumanGateRunner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        if ctx.cancel.is_cancelled() {
            return Ok(NodeOutcome::skipped());
        }

        let approved = ctx
            .inputs
            .values()
            .any(|v| v.get("approved").and_then(|x| x.as_bool()).unwrap_or(false));

        if approved {
            return Ok(NodeOutcome::success(json!({
                "approved": true,
                "message": self.message,
            })));
        }

        Ok(NodeOutcome {
            status: NodeStatus::Failed("human gate: not approved".into()),
            outputs: json!({ "approved": false, "message": self.message }),
            error: Some("human gate: awaiting approval".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use reflect_subagent::SubAgentFactory;
    use reflect_task::TaskManager;
    use reflect_task::{InMemoryTaskStore, InMemoryTeamStore};
    use tokio_util::sync::CancellationToken;

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

    fn ctx(inputs: HashMap<String, serde_json::Value>) -> NodeContext {
        NodeContext {
            name: "gate".into(),
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

    #[tokio::test]
    async fn gate_passes_when_upstream_approved() {
        let mut inputs = HashMap::new();
        inputs.insert("plan".into(), json!({"approved": true}));
        let runner = HumanGateRunner::new("gate", "ok");
        let out = runner.run(&ctx(inputs)).await.unwrap();
        assert!(out.status.is_success());
    }

    #[tokio::test]
    async fn gate_fails_without_approval() {
        let runner = HumanGateRunner::new("gate", "wait");
        let out = runner.run(&ctx(HashMap::new())).await.unwrap();
        assert!(out.status.is_failure());
    }
}
