//! `team_preset` —— 团队 Pipeline 预设(P2 `team-pipeline`)。
//!
//! 通过 `TaskManager::get_team` 加载团队,跑 plan → prd → exec → verify。

use std::collections::HashMap;
use std::sync::Arc;

use reflect_subagent::SubAgentFactory;
use reflect_task::{TaskError, TaskManager, TeamFile};
use tokio_util::sync::CancellationToken;

use crate::{
    FailurePolicy, PRESET_EXECUTOR_NAME, PRESET_PLANNER_NAME, PRESET_PRD_NAME,
    PRESET_VERIFIER_NAME, Pipeline, PipelineContext, PipelineReport, TeamNodeRunner, executor_node,
    planner_node, prd_node, verifier_node,
};

/// 默认团队 pipeline TOML(四节点链)。
pub const TEAM_PIPELINE_TOML: &str = r#"
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

/// 用已注册团队名触发四阶段 pipeline。
pub async fn run_team_pipeline(
    manager: Arc<TaskManager>,
    team_name: &str,
    topic: String,
    factory: Arc<SubAgentFactory>,
    cancel: CancellationToken,
) -> Result<PipelineReport, TaskError> {
    let team = manager.get_team(team_name).await?;
    run_team_pipeline_with_team(team, topic, factory, manager, cancel)
        .await
        .map_err(|e| TaskError::Invalid(e.to_string()))
}

/// `TaskManager` 扩展 —— 触发团队 pipeline 预设。
pub trait TeamPipelineExt {
    fn run_team_pipeline(
        self: &Arc<Self>,
        team_name: &str,
        topic: String,
        factory: Arc<SubAgentFactory>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<PipelineReport, TaskError>> + Send;
}

impl TeamPipelineExt for TaskManager {
    fn run_team_pipeline(
        self: &Arc<Self>,
        team_name: &str,
        topic: String,
        factory: Arc<SubAgentFactory>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<PipelineReport, TaskError>> + Send {
        run_team_pipeline(Arc::clone(self), team_name, topic, factory, cancel)
    }
}

/// 已知 `TeamFile` 时直接跑 pipeline(测试 / CLI)。
pub async fn run_team_pipeline_with_team(
    team: TeamFile,
    topic: impl Into<String>,
    factory: Arc<SubAgentFactory>,
    manager: Arc<TaskManager>,
    cancel: CancellationToken,
) -> Result<PipelineReport, crate::PipelineError> {
    let team_arc = Arc::new(team);
    let pipeline = Pipeline::from_toml(TEAM_PIPELINE_TOML, |label, _runner, _| {
        Some(match label {
            PRESET_PLANNER_NAME => {
                Arc::new(planner_node(label, (*team_arc).clone())) as Arc<dyn crate::NodeRunner>
            }
            PRESET_PRD_NAME => Arc::new(prd_node(label, (*team_arc).clone())),
            PRESET_EXECUTOR_NAME => Arc::new(executor_node(label, (*team_arc).clone())),
            PRESET_VERIFIER_NAME => Arc::new(verifier_node(label, (*team_arc).clone())),
            other => Arc::new(TeamNodeRunner::new(other, (*team_arc).clone(), "{{topic}}")),
        })
    })?
    .with_failure_policy(FailurePolicy::Abort);

    let ctx = PipelineContext {
        topic: topic.into(),
        inputs: HashMap::new(),
        factory,
        manager,
        cancel,
        human_gate: None,
    };
    pipeline.run(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_task::{InMemoryTaskStore, InMemoryTeamStore};
    use std::time::SystemTime;

    use reflect_task::TeamMemberSpec;

    fn dummy_team() -> TeamFile {
        TeamFile {
            name: "rocket".into(),
            description: None,
            lead_agent_id: "lead@rocket".into(),
            lead_session_id: None,
            members: vec![TeamMemberSpec {
                agent_id: "lead@rocket".into(),
                name: "lead".into(),
                role: "team-lead".into(),
                model: None,
                system_prompt: "lead".into(),
                allowed_tools: vec![],
                color: None,
                joined_at: SystemTime::now(),
                session_id: None,
                subscriptions: vec![],
            }],
            created_at: SystemTime::now(),
        }
    }

    fn dummy_factory() -> Arc<SubAgentFactory> {
        use reflect_llm::ModelRegistry;
        use reflect_protocol::ThreadId;
        use reflect_tools::ToolRegistry;
        Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            Arc::new(ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ))
    }

    #[tokio::test]
    async fn team_pipeline_toml_parses_four_nodes() {
        let p = Pipeline::from_toml(TEAM_PIPELINE_TOML, |label, _runner, _| {
            Some(
                Arc::new(TeamNodeRunner::new(label, dummy_team(), "{{topic}}"))
                    as Arc<dyn crate::NodeRunner>,
            )
        })
        .unwrap();
        assert_eq!(p.node_count(), 4);
    }

    #[tokio::test]
    async fn run_team_pipeline_requires_team() {
        let mgr = Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let err = run_team_pipeline(
            mgr,
            "missing",
            "topic".to_string(),
            dummy_factory(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TaskError::TeamNotFound(_)));
    }
}
