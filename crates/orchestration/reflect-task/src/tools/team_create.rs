//! `TeamCreate` — 创建一个 team 并写入 `~/.reflect/teams/<name>.json`。
//!
//! 创建契约:
//! - 必填 `name`(1..=64 字符,字符集 `[a-z0-9_-]`,与 `SubAgentSpec::validate` 对齐)。
//! - 可选 `description`。
//! - 可选 `members: Vec<TeamMemberSpec>`,缺省时仅含 1 个 `team-lead@<name>` 成员。
//!
//! 返回:`text = "Team '<name>' created with N members (lead: <lead_agent_id>)"`;
//! `metadata.team` 携带完整 `TeamFile` 快照,`metadata.leadAgentId` 平铺便于
//! reducer(Phase 6 TUI 团队胶囊)直接消费。
//!
//! ## 权限与并发
//!
//! `required_permission = Prompt` —— 写盘 + 创建 side-effecting resources;
//! `is_concurrency_safe = false` —— 同名 team 二次创建需由 manager 拒绝,
//! 跨调用顺序敏感(LLM 可能先创建再追加成员)。

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::model::{TeamFile, TeamMemberSpec};
use crate::team::{lead_agent_id_for, validate_team_name};
use crate::tools::{parse_optional_string, parse_string_arg};

/// `TeamCreate` 工具实现。
pub struct TeamCreateTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TeamCreateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamCreateTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TeamCreateTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

/// 从单条 JSON 解析 `TeamMemberSpec`。必填 `agent_id` + `name`,其余
/// 字段可选。
fn parse_member(v: &Value) -> Result<TeamMemberSpec, ToolError> {
    let agent_id = v
        .get("agent_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: "members[].agent_id missing or non-string".into(),
        })?
        .to_string();
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("member '{agent_id}' missing 'name'"),
        })?
        .to_string();
    let role = v
        .get("role")
        .and_then(|x| x.as_str())
        .unwrap_or(&name)
        .to_string();
    let model = parse_optional_string(v, "model");
    let system_prompt = parse_optional_string(v, "system_prompt").unwrap_or_default();
    let allowed_tools = v
        .get("allowed_tools")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let color = parse_optional_string(v, "color");
    // `agent_id` 格式校验:必须含 `@` 且前半段(角色)符合 `[a-z0-9_-]+`。
    if let Some((role_part, team_part)) = crate::team::parse_agent_id(&agent_id) {
        if role_part.is_empty() || team_part.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "member agent_id '{agent_id}' must be `<role>@<team>`, both non-empty"
                ),
            });
        }
        if !role_part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "member agent_id '{agent_id}' role part '{role_part}' must be [a-z0-9_-]+"
                ),
            });
        }
    } else {
        return Err(ToolError::InvalidArgs {
            message: format!(
                "member agent_id '{agent_id}' must contain '@' in `<role>@<team>` form"
            ),
        });
    }
    Ok(TeamMemberSpec {
        agent_id,
        name,
        role,
        model,
        system_prompt,
        allowed_tools,
        color,
        joined_at: SystemTime::now(),
        session_id: None,
        subscriptions: vec![],
    })
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }

    fn description(&self) -> &str {
        "创建一个 team(team-lead@<name> lead 必出,追加在用户成员之后)。\
         返回 team 快照 + lead_agent_id + memberCount。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "pattern": "^[a-z0-9_-]+$",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Team name; 1..=64 chars, lowercase + digits + _-"
                },
                "description": {
                    "type": "string",
                    "description": "Human-readable team description"
                },
                "members": {
                    "type": "array",
                    "description": "Initial team members (lead is appended automatically)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "Agent id in <role>@<team> form"
                            },
                            "name": {"type": "string"},
                            "role": {"type": "string"},
                            "model": {"type": "string"},
                            "system_prompt": {"type": "string"},
                            "allowed_tools": {
                                "type": "array",
                                "items": {"type": "string"}
                            },
                            "color": {"type": "string"}
                        },
                        "required": ["agent_id", "name"]
                    }
                }
            },
            "required": ["name"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 跨调用顺序敏感:同名 team 二次创建需由 manager 拒绝,LLM
        // 可能先创建再追加成员(Phase 3 才会落地 update_members 工具)。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // 写盘 + 创建持久化资源,需 Prompt 审批。
        PermissionMode::Prompt
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let name = parse_string_arg(&args, "name")?;
        // 二次校验:name 必须合法(防止 LLM 传 TeamCreate 内嵌 schema
        // 之外的字符,虽然 schema 已约束 pattern)。
        validate_team_name(&name).map_err(|e| ToolError::InvalidArgs {
            message: e.to_string(),
        })?;

        let description = parse_optional_string(&args, "description");
        let mut members: Vec<TeamMemberSpec> = args
            .get("members")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_member).collect::<Result<_, _>>())
            .transpose()?
            .unwrap_or_default();

        // lead 成员默认追加;若用户已显式提供同 agent_id(冲突),
        // 由 manager 进一步处理 —— 本工具不重复校验,以保持"lead 永远存在"
        // 的不变量。
        let lead_agent_id = lead_agent_id_for(&name);
        members.push(TeamMemberSpec {
            agent_id: lead_agent_id.clone(),
            name: "team-lead".into(),
            role: "team-lead".into(),
            model: None,
            system_prompt:
                "You are the team lead. Coordinate team members and own the team-level task list."
                    .into(),
            // Phase 4 才追加完整 allowed_tools(coordinator 模式);
            // Phase 2 留空 —— SubAgentFactory 后续走默认工具集。
            allowed_tools: vec![],
            color: None,
            joined_at: SystemTime::now(),
            session_id: None,
            subscriptions: vec![],
        });

        let team = TeamFile {
            name: name.clone(),
            description,
            lead_agent_id,
            lead_session_id: None,
            members,
            created_at: SystemTime::now(),
        };
        self.manager.upsert_team(team.clone()).await?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Team '{name}' created with {} members (lead: {}).",
                team.members.len(),
                team.lead_agent_id
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "team": &team,
                "teamName": team.name,
                "leadAgentId": team.lead_agent_id,
                "memberCount": team.members.len(),
                // v1.1.0 Phase 4:TUI status_bar 读 activeWorkers;创建时尚无 worker。
                "activeWorkers": 0,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryTaskStore, InMemoryTeamStore};
    use reflect_tools::Tool;

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    #[tokio::test]
    async fn create_team_default_lead_only() {
        let tool = TeamCreateTool::new(mgr());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "rocket", "description": "build rockets"}),
            )
            .await
            .unwrap();
        let meta = &out.metadata;
        assert_eq!(meta["teamName"], "rocket");
        assert_eq!(meta["leadAgentId"], "team-lead@rocket");
        assert_eq!(meta["memberCount"], 1);
        let team = meta["team"].as_object().unwrap();
        assert_eq!(team["members"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_team_with_user_members() {
        let tool = TeamCreateTool::new(mgr());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "name": "rocket",
                    "members": [
                        {
                            "agent_id": "architect@rocket",
                            "name": "architect",
                            "role": "architect",
                            "model": "anthropic/claude-opus-4-6",
                            "system_prompt": "design",
                            "allowed_tools": ["Read", "Write"]
                        },
                        {
                            "agent_id": "builder@rocket",
                            "name": "builder",
                            "role": "builder"
                        }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["memberCount"], 3); // 2 user + 1 lead
        let team = out.metadata["team"].as_object().unwrap();
        let ids: Vec<&str> = team["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["agent_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"architect@rocket"));
        assert!(ids.contains(&"builder@rocket"));
        assert!(ids.contains(&"team-lead@rocket"));
    }

    #[tokio::test]
    async fn create_team_rejects_uppercase_name() {
        let tool = TeamCreateTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "Rocket"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn create_team_rejects_missing_name() {
        let tool = TeamCreateTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn create_team_rejects_member_without_at_sign() {
        let tool = TeamCreateTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "name": "rocket",
                    "members": [{"agent_id": "noatsign", "name": "x"}]
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn create_team_rejects_member_uppercase_role() {
        let tool = TeamCreateTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "name": "rocket",
                    "members": [{"agent_id": "Architect@rocket", "name": "x"}]
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn metadata_is_stable() {
        let tool = TeamCreateTool::new(mgr());
        assert_eq!(tool.name(), "TeamCreate");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
    }
}
