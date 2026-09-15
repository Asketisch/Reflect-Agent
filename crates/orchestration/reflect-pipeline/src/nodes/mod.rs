//! `nodes` — `TeamNodeRunner` + 4 阶段预设(planner / prd / executor / verifier)。
//!
//! `TeamNodeRunner` 把 `TeamFile` 的成员 spec 注入 `SubAgentFactory.dynamic_specs`,
//! 用 `task_template` 渲染 prompt,spawn + drain `SpawnedChild::collect_result_with_usage`,
//! 把 `SpawnedResult.text` 解析为 `outputs.result`。
//!
//! # 4 阶段预设
//!
//! | 预设名 | 默认 prompt 模板(模板字符串为字面量) | 输出字段 |
//! |---|---|---|
//! | `planner` | `"Plan for: {{topic}}\n\n{{input.audience}}"` | `result` |
//! | `prd` | `"PRD from plan: {{nodes.plan.outputs.result}}"` | `result` |
//! | `executor` | `"Implement: {{nodes.prd.outputs.result}}"` | `result` |
//! | `verifier` | `"Verify: {{nodes.exec.outputs.result}}"` | `result` |
//!
//! 模板可被用户覆盖(传 `template: "..."` 到 `TeamNodeRunner::with_template`)。

pub mod human_gate;
pub mod join;
pub mod loop_control;
pub mod shell;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use reflect_subagent::SubAgentFactory;
use reflect_task::{TeamFile, TeamMemberSpec};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::error::PipelineError;
use crate::nodes::shell::{ShellNodeParams, ShellNodeRunner};
use crate::runner::{NodeContext, NodeOutcome, NodeRunner, NodeStatus};
use crate::template;

/// 预设节点名 —— 与 TOML `[nodes.<name>]` 段对应。
pub const PRESET_PLANNER_NAME: &str = "plan";
pub const PRESET_PRD_NAME: &str = "prd";
pub const PRESET_EXECUTOR_NAME: &str = "exec";
pub const PRESET_VERIFIER_NAME: &str = "verify";

/// `TeamNodeRunner` 配置参数(TOML 解析)。
///
/// ```toml
/// [nodes.plan]           # 节点段名(plan / prd / exec / verify)
/// runner = "team"        # 节点 runner 类型
///
/// [nodes.plan.params]    # 节点参数子段
/// template = "Plan for {{topic}}"   # 任务 prompt 模板
/// team = "planner-team"   # 可选:从 TaskManager 取已注册 team;None 时 prompt-only
/// role = "planner"        # 可选:从 team 里挑哪个角色 spawn
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct TeamNodeParams {
    /// task prompt 模板。`{{topic}}` / `{{input.X}}` / `{{nodes.X.outputs.Y}}` 占位符可用。
    #[serde(default)]
    pub template: Option<String>,
    /// 团队名(v1.1.0 暂不实现 team 加载,留字段给未来 PR)。
    #[serde(default)]
    pub team: Option<String>,
    /// spawn 哪个 role 的成员。
    #[serde(default)]
    pub role: Option<String>,
}

/// 把 TeamMemberSpec 转换成 spec 字段(供 SubAgentFactory::add_spec 用)。
///
/// 不直接走 `reflect_task::From<&TeamMemberSpec> for SubAgentSpec` —— 该
/// trait 在 Phase 3 实现,本节点 runner 不依赖具体 spec 转换,而是把
/// `team.members[*]` 透传到 `factory.add_spec`,由 factory 校验。
fn member_to_spec(member: &TeamMemberSpec) -> reflect_subagent::SubAgentSpec {
    use reflect_subagent::data_transfer::DataTransferConfig;
    reflect_subagent::SubAgentSpec {
        name: member.name.clone(),
        role: if member.role.is_empty() {
            member.name.clone()
        } else {
            member.role.clone()
        },
        model: member.model.clone(),
        system_prompt: member.system_prompt.clone(),
        allowed_tools: member.allowed_tools.clone(),
        data_transfer: DataTransferConfig::default(),
        max_turns: None,
        allowed_skills: vec![],
    }
}

/// 把团队成员 spec 注入 `SubAgentFactory`,供本节点 spawn 使用。
pub fn inject_team(factory: &SubAgentFactory, team: &TeamFile) -> usize {
    let mut count = 0;
    for m in &team.members {
        if factory.add_spec(member_to_spec(m)) {
            count += 1;
        } else {
            warn!(
                role = %m.role,
                agent_id = %m.agent_id,
                "TeamNodeRunner: 成员 spec validate 失败,跳过"
            );
        }
    }
    count
}

/// 单节点 = 一个 team 调用。
///
/// 持有 `team` 成员列表 + `task_template`,运行时:
/// 1. 把 team 成员 spec 注入 `ctx.factory`。
/// 2. 用 `template::render` 渲染 prompt。
/// 3. `factory.spawn(spec, ..., prompt)` → drain → 解析为 outputs。
pub struct TeamNodeRunner {
    pub name: String,
    pub team: TeamFile,
    pub task_template: String,
    /// spawn 哪个 role。`None` = 选团队里第一个非 lead 成员,或 lead(单成员)。
    pub subagent_role: Option<String>,
    /// 模板渲染额外 input(`{{input.<key>}}`)。
    pub inputs: HashMap<String, String>,
}

impl std::fmt::Debug for TeamNodeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamNodeRunner")
            .field("name", &self.name)
            .field("team", &self.team.name)
            .field("task_template", &self.task_template)
            .field("subagent_role", &self.subagent_role)
            .finish()
    }
}

impl TeamNodeRunner {
    /// 构造 runner(默认 subagent_role = 团队 lead)。
    pub fn new(name: impl Into<String>, team: TeamFile, task_template: impl Into<String>) -> Self {
        let lead_role = team
            .lead_agent_id
            .split('@')
            .next()
            .unwrap_or("team-lead")
            .to_string();
        Self {
            name: name.into(),
            team,
            task_template: task_template.into(),
            subagent_role: Some(lead_role),
            inputs: HashMap::new(),
        }
    }

    /// 显式指定 spawn role。
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.subagent_role = Some(role.into());
        self
    }

    /// 注入额外 input 字段。
    pub fn with_input(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inputs.insert(key.into(), value.into());
        self
    }

    /// 从 `TeamNodeParams` 解析(给 `Pipeline::from_toml` 用)。
    ///
    /// `team_for_label` 闭包传 label → `Option<TeamFile>`,允许 CLI 加载
    /// 已注册团队。本 crate 不直接连 `TaskManager`,把团队查找留给 caller。
    pub fn from_params(
        name: impl Into<String>,
        params: &TeamNodeParams,
        default_template: &str,
        team_for_label: impl FnOnce(&str) -> Option<TeamFile>,
    ) -> Result<Self, PipelineError> {
        let name = name.into();
        let team = params
            .team
            .as_deref()
            .and_then(team_for_label)
            .ok_or_else(|| {
                PipelineError::Config(format!(
                    "TeamNodeRunner '{name}': params.team missing or unknown"
                ))
            })?;
        let template = params
            .template
            .clone()
            .unwrap_or_else(|| default_template.to_string());
        let mut runner = Self::new(name, team, template);
        if let Some(role) = &params.role {
            runner = runner.with_role(role.clone());
        }
        Ok(runner)
    }
}

#[async_trait]
impl NodeRunner for TeamNodeRunner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        // 1. 注入 team 成员 spec 到 factory。
        let injected = inject_team(&ctx.factory, &self.team);
        debug!(
            node = %self.name,
            team = %self.team.name,
            injected,
            "TeamNodeRunner: injected team specs"
        );

        // 2. 渲染 prompt。
        let upstream: HashMap<String, serde_json::Value> = ctx.inputs.clone();
        let prompt = template::render(&self.task_template, &ctx.topic, &self.inputs, &upstream)
            .map_err(|e| {
                PipelineError::node_msg(self.name.clone(), format!("template render failed: {e}"))
            })?;

        // 3. 选 role(优先 subagent_role,否则团队 lead)。
        let role = self
            .subagent_role
            .clone()
            .or_else(|| {
                Some(
                    self.team
                        .lead_agent_id
                        .split('@')
                        .next()
                        .unwrap_or("team-lead")
                        .to_string(),
                )
            })
            .expect("role default is Some");

        // 4. 取出 spec(由 step 1 注入)。
        let spec = ctx.factory.get_spec(&role).ok_or_else(|| {
            PipelineError::node_msg(
                self.name.clone(),
                format!("role '{role}' not found in factory after inject"),
            )
        })?;

        // 5. spawn 子代理并排空结果。
        let started = std::time::Instant::now();
        let spawned = ctx
            .factory
            .spawn(spec, vec![], prompt.clone())
            .await
            .map_err(|e| PipelineError::node_msg(self.name.clone(), e.to_string()))?;
        let result = spawned
            .collect_result()
            .await
            .map_err(|e| PipelineError::node_msg(self.name.clone(), e.to_string()))?;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        debug!(
            node = %self.name,
            elapsed_ms,
            "TeamNodeRunner: spawned + drained"
        );

        // 6. 构造 outputs。
        let outputs = serde_json::json!({
            "result": result,
            "elapsed_ms": elapsed_ms,
            "team": self.team.name,
            "role": role,
        });
        Ok(NodeOutcome {
            status: NodeStatus::Success,
            outputs,
            error: None,
        })
    }
}

// ── 4 阶段预设工厂 ─────────────────────────────────────────────────────────

/// `planner` 预设 — `Plan for {{topic}}`。
pub fn planner_node(name: impl Into<String>, team: TeamFile) -> TeamNodeRunner {
    TeamNodeRunner::new(name, team, "Plan for: {{topic}}\n\n{{input.audience}}")
}

/// `prd` 预设 — `PRD from plan: {{nodes.plan.outputs.result}}`。
pub fn prd_node(name: impl Into<String>, team: TeamFile) -> TeamNodeRunner {
    TeamNodeRunner::new(name, team, "PRD from plan: {{nodes.plan.outputs.result}}")
}

/// `executor` 预设 — `Implement: {{nodes.prd.outputs.result}}`。
pub fn executor_node(name: impl Into<String>, team: TeamFile) -> TeamNodeRunner {
    TeamNodeRunner::new(name, team, "Implement: {{nodes.prd.outputs.result}}")
}

/// `verifier` 预设 — `Verify: {{nodes.exec.outputs.result}}`。
pub fn verifier_node(name: impl Into<String>, team: TeamFile) -> TeamNodeRunner {
    TeamNodeRunner::new(name, team, "Verify: {{nodes.exec.outputs.result}}")
}

/// v1.4 C2:默认 runner 分发器 —— `Pipeline::from_toml` 的 runner_for
/// 闭包推荐实现。按 TOML 的 `runner` 字段分发:
/// - `"shell"` → [`ShellNodeRunner::from_params`](解析 params.command);
/// - 其余(`"team"` 等)→ 走 label 预设(`preset_for`)。
///
/// 闭包签名变更(v1.4):runner 类型透传给分发器,不再靠 params 内容猜。
pub fn default_runner_for(
    label: &str,
    runner: &str,
    params: Option<&toml::Value>,
    team_for_label: impl Fn(&str) -> Option<TeamFile>,
) -> Option<Arc<dyn NodeRunner>> {
    if runner == "shell" {
        let p = params?;
        let parsed: ShellNodeParams = p.clone().try_into().ok()?;
        return Some(Arc::new(ShellNodeRunner::from_params(label, &parsed).ok()?));
    }
    let _ = runner;
    preset_for(label, team_for_label)
}

/// 预设注册器 —— 帮 `Pipeline::from_toml` 的 runner_for 闭包根据节点 label
/// 自动选预设。当节点 label ∈ `{plan, prd, exec, verify}` 且未在 params
/// 显式提供 template 时,使用预设;否则使用 params.template。
///
/// 给 caller 提供一个统一入口,不需要在每个 pipeline 配置里重写 4 段模板。
pub fn preset_for(
    label: &str,
    team_for_label: impl Fn(&str) -> Option<TeamFile>,
) -> Option<Arc<dyn NodeRunner>> {
    let team = team_for_label(label)?;
    let arc: Arc<dyn NodeRunner> = match label {
        PRESET_PLANNER_NAME => Arc::new(planner_node(label, team)),
        PRESET_PRD_NAME => Arc::new(prd_node(label, team)),
        PRESET_EXECUTOR_NAME => Arc::new(executor_node(label, team)),
        PRESET_VERIFIER_NAME => Arc::new(verifier_node(label, team)),
        _ => return None,
    };
    Some(arc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_subagent::SubAgentSpec;
    use std::time::SystemTime;

    fn dummy_team() -> TeamFile {
        TeamFile {
            name: "rocket".into(),
            description: Some("rocket team".into()),
            lead_agent_id: "team-lead@rocket".into(),
            lead_session_id: None,
            members: vec![TeamMemberSpec {
                agent_id: "team-lead@rocket".into(),
                name: "team-lead".into(),
                role: "team-lead".into(),
                model: None,
                system_prompt: "you are a planner".into(),
                allowed_tools: vec![],
                color: None,
                joined_at: SystemTime::now(),
                session_id: None,
                subscriptions: vec![],
            }],
            created_at: SystemTime::now(),
        }
    }

    #[test]
    fn new_runner_uses_lead_role_by_default() {
        let runner = TeamNodeRunner::new("plan", dummy_team(), "{{topic}}");
        assert_eq!(runner.subagent_role.as_deref(), Some("team-lead"));
    }

    #[test]
    fn with_role_overrides() {
        let runner = TeamNodeRunner::new("plan", dummy_team(), "{{topic}}").with_role("planner");
        assert_eq!(runner.subagent_role.as_deref(), Some("planner"));
    }

    #[test]
    fn with_input_adds_field() {
        let runner = TeamNodeRunner::new("plan", dummy_team(), "{{topic}}")
            .with_input("audience", "engineers");
        assert_eq!(
            runner.inputs.get("audience").map(String::as_str),
            Some("engineers")
        );
    }

    #[test]
    fn inject_team_registers_each_member() {
        use parking_lot::Mutex;
        use reflect_llm::ModelRegistry;
        use reflect_protocol::ThreadId;
        use reflect_tools::ToolRegistry;
        use tokio_util::sync::CancellationToken;

        let factory = SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            Arc::new(ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        );
        let _ = Mutex::new(());
        let n = inject_team(&factory, &dummy_team());
        assert_eq!(n, 1, "1 个 lead 成员被注入");
        assert!(factory.get_spec("team-lead").is_some());
    }

    #[test]
    fn preset_for_returns_4_presets() {
        let team = dummy_team();
        for label in [
            PRESET_PLANNER_NAME,
            PRESET_PRD_NAME,
            PRESET_EXECUTOR_NAME,
            PRESET_VERIFIER_NAME,
        ] {
            let r = preset_for(label, |_| Some(team.clone()));
            assert!(r.is_some(), "preset '{label}' should return runner");
        }
        // 未知 label → None
        let r = preset_for("ghost", |_| Some(team.clone()));
        assert!(r.is_none());
    }

    #[test]
    fn preset_for_returns_none_when_team_missing() {
        let r = preset_for(PRESET_PLANNER_NAME, |_| None);
        assert!(r.is_none());
    }

    #[test]
    fn from_params_requires_team() {
        let params = TeamNodeParams {
            template: Some("custom".into()),
            team: None,
            role: None,
        };
        let err = TeamNodeRunner::from_params("plan", &params, "default", |_| None).unwrap_err();
        assert!(matches!(err, PipelineError::Config(_)));
    }

    #[test]
    fn from_params_uses_default_when_no_template() {
        let params = TeamNodeParams {
            template: None,
            team: Some("rocket".into()),
            role: None,
        };
        let runner = TeamNodeRunner::from_params("plan", &params, "default {{topic}}", |_| {
            Some(dummy_team())
        })
        .unwrap();
        assert_eq!(runner.task_template, "default {{topic}}");
    }

    #[test]
    fn from_params_uses_override_template() {
        let params = TeamNodeParams {
            template: Some("custom {{topic}}".into()),
            team: Some("rocket".into()),
            role: Some("planner".into()),
        };
        let runner =
            TeamNodeRunner::from_params("plan", &params, "default", |_| Some(dummy_team()))
                .unwrap();
        assert_eq!(runner.task_template, "custom {{topic}}");
        assert_eq!(runner.subagent_role.as_deref(), Some("planner"));
    }

    /// 成员 spec 转换字段直传。
    #[test]
    fn member_to_spec_passes_through() {
        let team = dummy_team();
        let spec: SubAgentSpec = member_to_spec(&team.members[0]);
        assert_eq!(spec.role, "team-lead");
        assert_eq!(spec.name, "team-lead");
        assert_eq!(spec.system_prompt, "you are a planner");
    }
}
