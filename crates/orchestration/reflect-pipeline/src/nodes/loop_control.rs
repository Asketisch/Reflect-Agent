//! `loop_control` —— Loop 回退(P2 `pipeline-loop`)。
//!
//! 读取上游 `passed` / `ok` 字段;失败且未达 `loop_max` 时标记
//! `loop_to` 供 DAG 编排器重入。

use async_trait::async_trait;
use serde_json::json;

use crate::error::PipelineError;
use crate::runner::{NodeContext, NodeOutcome, NodeRunner};

/// Loop 控制参数(TOML `[nodes.X.params]`)。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LoopParams {
    /// 回退目标节点 label。
    #[serde(default)]
    pub loop_to: Option<String>,
    /// 最大循环次数(含首次)。
    #[serde(default = "default_loop_max")]
    pub loop_max: u32,
}

fn default_loop_max() -> u32 {
    3
}

/// Loop 检查节点 —— 验证上游结果,输出 loop 决策 metadata。
pub struct LoopControlRunner {
    pub name: String,
    pub loop_to: Option<String>,
    pub loop_max: u32,
    pub iteration: u32,
}

impl LoopControlRunner {
    pub fn new(name: impl Into<String>, params: &LoopParams, iteration: u32) -> Self {
        Self {
            name: name.into(),
            loop_to: params.loop_to.clone(),
            loop_max: params.loop_max,
            iteration,
        }
    }
}

#[async_trait]
impl NodeRunner for LoopControlRunner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        if ctx.cancel.is_cancelled() {
            return Ok(NodeOutcome::skipped());
        }

        // 迭代号优先取 ctx(Pipeline::run 的 loop 跟踪注入);ctx 默认 1,
        // 兼容老调用方(self.iteration 仅作 fallback,多为测试用)。
        let iteration = if ctx.iteration > 0 {
            ctx.iteration
        } else {
            self.iteration
        };

        let passed = ctx.inputs.values().any(|v| {
            v.get("passed")
                .or_else(|| v.get("ok"))
                .and_then(|x| x.as_bool())
                .unwrap_or(true)
        });

        if passed {
            return Ok(NodeOutcome::success(json!({
                "passed": true,
                "iteration": iteration,
                "loop_to": null,
            })));
        }

        let can_loop = iteration < self.loop_max;
        let loop_to = if can_loop { self.loop_to.clone() } else { None };

        if can_loop && loop_to.is_some() {
            Ok(NodeOutcome::success(json!({
                "passed": false,
                "iteration": iteration,
                "loop_to": loop_to,
                "should_loop": true,
            })))
        } else {
            Ok(NodeOutcome::failure(format!(
                "loop max ({}) exceeded or no loop_to",
                self.loop_max
            )))
        }
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
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        )
    }

    fn ctx(inputs: HashMap<String, serde_json::Value>) -> NodeContext {
        NodeContext {
            name: "loop".into(),
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
    async fn loop_passes_when_upstream_ok() {
        let mut inputs = HashMap::new();
        inputs.insert("verify".into(), json!({"passed": true}));
        let params = LoopParams {
            loop_to: Some("plan".into()),
            loop_max: 3,
        };
        let runner = LoopControlRunner::new("loop", &params, 1);
        let out = runner.run(&ctx(inputs)).await.unwrap();
        assert!(out.status.is_success());
        assert_eq!(out.outputs["passed"], true);
    }

    #[tokio::test]
    async fn loop_requests_retry_when_failed() {
        let mut inputs = HashMap::new();
        inputs.insert("verify".into(), json!({"passed": false}));
        let params = LoopParams {
            loop_to: Some("plan".into()),
            loop_max: 3,
        };
        let runner = LoopControlRunner::new("loop", &params, 1);
        let out = runner.run(&ctx(inputs)).await.unwrap();
        assert!(out.status.is_success());
        assert_eq!(out.outputs["should_loop"], true);
        assert_eq!(out.outputs["loop_to"], "plan");
    }
}
