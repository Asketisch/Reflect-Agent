//! `TeamDelete` — 物理删除 team 文件(不留 tombstone)。
//!
//! 与 `TaskUpdate status=deleted` 同理 —— 软删仅影响 metadata 字段,本工具
//! 是物理删除 `~/.reflect/teams/<name>.json`。
//!
//! **不级联删除 team tasks** —— 任务文件留在 `<home>/tasks/<name>/<id>.json`,
//! 由 `reflect task purge` 子命令(Phase 3 CLI)统一清理,避免 `TeamDelete`
//! 误删 worker 仍在执行的任务。
//!
//! `required_permission = Prompt` —— 删盘是高敏感操作,需审批。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;

use crate::manager::TaskManager;
use crate::team::validate_team_name;
use crate::tools::parse_string_arg;

/// `TeamDelete` 工具实现。
pub struct TeamDeleteTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TeamDeleteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamDeleteTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TeamDeleteTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }

    fn description(&self) -> &str {
        "物理删除指定 team。team 关联的任务列表保留(独立用 task purge 显式清理)。"
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
                    "description": "Team name to delete"
                }
            },
            "required": ["name"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 删盘操作,跨调用顺序敏感(若用户接着 get_team,顺序有要求)。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let name = parse_string_arg(&args, "name")?;
        validate_team_name(&name).map_err(|e| ToolError::InvalidArgs {
            message: e.to_string(),
        })?;

        // 先 read 再 delete —— 把 "was N members" 写进响应,便于 LLM / 日志
        // 调试。若 team 不存在,`get_team` 返 `TeamNotFound`,直接透传。
        let existed = self.manager.get_team(&name).await.ok();
        let prior_count = existed.as_ref().map(|t| t.members.len()).unwrap_or(0);

        self.manager.delete_team(&name).await?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Team '{name}' deleted (was {prior_count} members)."
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "teamName": name,
                "deletedMemberCount": prior_count,
                "existed": existed.is_some(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::TaskManager;
    use crate::model::{TeamFile, TeamMemberSpec};
    use crate::{InMemoryTaskStore, InMemoryTeamStore};
    use reflect_tools::Tool;
    use std::time::SystemTime;

    fn mgr() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ))
    }

    fn make_team(name: &str, members: usize) -> TeamFile {
        let now = SystemTime::now();
        let lead = crate::team::lead_agent_id_for(name);
        let mut ms = Vec::with_capacity(members);
        for i in 0..members {
            ms.push(TeamMemberSpec {
                agent_id: format!("member-{i}@{name}"),
                name: format!("m{i}"),
                role: format!("member-{i}"),
                model: None,
                system_prompt: String::new(),
                allowed_tools: vec![],
                color: None,
                joined_at: now,
                session_id: None,
                subscriptions: vec![],
            });
        }
        TeamFile {
            name: name.into(),
            description: None,
            lead_agent_id: lead,
            lead_session_id: None,
            members: ms,
            created_at: now,
        }
    }

    #[tokio::test]
    async fn delete_existing_team_reports_member_count() {
        let m = mgr();
        m.upsert_team(make_team("rocket", 2)).await.unwrap();
        let tool = TeamDeleteTool::new(m.clone());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "rocket"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["deletedMemberCount"], 2);
        assert_eq!(out.metadata["existed"], true);
        // 之后 get_team 返 NotFound
        let err = m.get_team("rocket").await.unwrap_err();
        assert!(matches!(err, crate::error::TaskError::TeamNotFound(_)));
    }

    #[tokio::test]
    async fn delete_missing_team_is_ok() {
        let m = mgr();
        let tool = TeamDeleteTool::new(m);
        // 不存在的 team 仍走 delete 路径,store 对 NotFound 容错。
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"name": "ghost"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["deletedMemberCount"], 0);
        assert_eq!(out.metadata["existed"], false);
    }

    #[tokio::test]
    async fn delete_rejects_invalid_name() {
        let tool = TeamDeleteTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"name": "BadName"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn delete_requires_name() {
        let tool = TeamDeleteTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TeamDeleteTool::new(mgr());
        assert_eq!(tool.name(), "TeamDelete");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
    }
}
