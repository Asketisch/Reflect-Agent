//! `cron_tool` — v1.2 P1-2 把 cron 调度暴露为 agent 工具。
//!
//! 单工具多 action(镜像 `reflect-ast::AstTool` 的 discriminated-action
//! 模式):`create` / `delete` / `list` / `get` / `update`。底层
//! `reflect_stream::CronScheduler` 持有 session-scoped job 列表,后台
//! driver 到期时把 `prompt` 作为 `Submission::user_input` 注入 agent loop。
//!
//! 权限路由:`list` / `get` → `Auto`(只读);`create` / `delete` / `update`
//! → `Prompt`(改变调度状态,需用户确认)。
//!
//! **注册状态:已在主 bootstrap 接线** —— `reflect-exec::headless` 构造
//! `CronScheduler`(绑定 submission sender,到期把 job.prompt 作为
//! `Submission::user_input` 注入)并注册 `CronTool`,随后 `start(30)`
//! 启动 30s tick 的后台 driver。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolError, ToolOutput};
use reflect_stream::CronScheduler;
use reflect_tools::{Tool, ToolContext};

/// agent 可调用的 cron 管理工具。持有一个共享的 `CronScheduler`。
#[derive(Clone)]
pub struct CronTool {
    scheduler: Arc<CronScheduler>,
}

impl std::fmt::Debug for CronTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronTool")
            .field("scheduler", &self.scheduler)
            .finish()
    }
}

impl CronTool {
    pub fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "在 cron 时间触发并将 prompt 作为新轮注入。操作:`create` / \
         `list` / `get` / `update` / `delete`。job 跨 turn 持久、session 范围内生效。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "delete", "list", "get", "update"],
                    "description": "Which cron operation to perform."
                },
                "schedule": {
                    "type": "string",
                    "description": "5-field cron expression (create/update). e.g. '0 * * * *' hourly, '*/15 * * * *' every 15min, '0 9 * * 1-5' weekdays 9am."
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt injected as a new turn when the job fires (create/update)."
                },
                "name": {
                    "type": "string",
                    "description": "Optional human-readable job name (create/update)."
                },
                "id": {
                    "type": "string",
                    "description": "Job id (get/update/delete)."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Enable/disable the job without deleting it (update)."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 改调度状态(有副作用),保守串行。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // Tool-level fallback:`list`/`get` 走 Auto;`create`/`delete`/`update`
        // 在 `action_permission` 切到 Prompt。
        PermissionMode::Auto
    }

    fn action_permission(&self, args: &serde_json::Value) -> PermissionMode {
        match args.get("action").and_then(|v| v.as_str()) {
            Some("create") | Some("delete") | Some("update") => PermissionMode::Prompt,
            _ => PermissionMode::Auto,
        }
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let action =
            args.get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "cron: missing required field 'action'".into(),
                })?;

        match action {
            "create" => self.exec_create(&args),
            "delete" => self.exec_delete(&args),
            "list" => self.exec_list(),
            "get" => self.exec_get(&args),
            "update" => self.exec_update(&args),
            other => Err(ToolError::InvalidArgs {
                message: format!("cron: unknown action '{other}'"),
            }),
        }
    }
}

impl CronTool {
    fn exec_create(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let schedule = require_str(args, "schedule")?;
        let prompt = require_str(args, "prompt")?;
        let name = args.get("name").and_then(|v| v.as_str());
        let job = self
            .scheduler
            .create(schedule, prompt, name.map(|s| s.to_string()))
            .map_err(|e| ToolError::InvalidArgs {
                message: format!("cron: invalid schedule: {e}"),
            })?;
        Ok(job_output(
            "created",
            Some(&job),
            serde_json::json!({ "id": job.id, "next_fire": job.next_fire }),
        ))
    }

    fn exec_delete(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let id = require_str(args, "id")?;
        let removed = self.scheduler.delete(id);
        Ok(job_output(
            if removed { "deleted" } else { "not_found" },
            None,
            serde_json::json!({ "id": id, "removed": removed }),
        ))
    }

    fn exec_list(&self) -> Result<ToolOutput, ToolError> {
        let jobs = self.scheduler.list();
        Ok(job_output(
            "list",
            None,
            serde_json::json!({
                "count": jobs.len(),
                "jobs": jobs.iter().map(job_summary).collect::<Vec<_>>(),
            }),
        ))
    }

    fn exec_get(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let id = require_str(args, "id")?;
        match self.scheduler.get(id) {
            Some(job) => Ok(job_output("get", Some(&job), serde_json::json!({}))),
            None => Ok(job_output(
                "not_found",
                None,
                serde_json::json!({ "id": id }),
            )),
        }
    }

    fn exec_update(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let id = require_str(args, "id")?;
        let schedule = args.get("schedule").and_then(|v| v.as_str());
        let prompt = args.get("prompt").and_then(|v| v.as_str());
        let name = args.get("name").and_then(|v| v.as_str());
        let enabled = args.get("enabled").and_then(|v| v.as_bool());
        // 若给了 schedule,先校验表达式可解析(update 内部也会校验,但提前
        // 给清晰错误信息)。
        if let Some(s) = schedule {
            if let Err(e) = reflect_stream::CronSchedule::parse(s) {
                return Err(ToolError::InvalidArgs {
                    message: format!("cron: invalid schedule: {e}"),
                });
            }
        }
        match self.scheduler.update(id, schedule, prompt, name, enabled) {
            Some(job) => Ok(job_output(
                "updated",
                Some(&job),
                serde_json::json!({ "id": job.id, "next_fire": job.next_fire }),
            )),
            None => Ok(job_output(
                "not_found",
                None,
                serde_json::json!({ "id": id }),
            )),
        }
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────

fn require_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("cron: missing required field '{key}'"),
        })
}

/// 把一条 job 渲染成精简 summary(给 list / 文本块用)。
fn job_summary(j: &reflect_stream::CronJobSpec) -> serde_json::Value {
    serde_json::json!({
        "id": j.id,
        "name": j.name,
        "schedule": j.schedule,
        "prompt": j.prompt,
        "enabled": j.enabled,
        "next_fire": j.next_fire,
        "last_fired": j.last_fired,
    })
}

/// 构造 `ToolOutput`:一个人类可读文本块 + 结构化 metadata。
fn job_output(
    action: &str,
    job: Option<&reflect_stream::CronJobSpec>,
    extra: serde_json::Value,
) -> ToolOutput {
    let text = match (action, job) {
        ("created", Some(j)) => format!(
            "Cron job created: {} — `{}`{}\n  next fire: {}",
            j.id,
            j.schedule,
            j.name
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default(),
            j.next_fire
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "unknown".into())
        ),
        ("deleted", _) => "Cron job deleted.".into(),
        ("not_found", _) => "Cron job not found.".into(),
        ("updated", Some(j)) => format!(
            "Cron job updated: {} — `{}` (enabled={})",
            j.id, j.schedule, j.enabled
        ),
        ("list", _) => "Cron jobs listed.".into(),
        ("get", Some(j)) => format!(
            "Cron job {}: `{}` — {}\n  prompt: {}\n  enabled={}",
            j.id,
            j.schedule,
            j.name.as_deref().unwrap_or("<unnamed>"),
            j.prompt,
            j.enabled
        ),
        _ => format!("Cron {action}."),
    };
    let mut metadata = serde_json::json!({ "action": action });
    if let Some(j) = job {
        metadata["job"] = job_summary(j);
    }
    if let serde_json::Value::Object(map) = extra {
        if let serde_json::Value::Object(meta) = &mut metadata {
            for (k, v) in map {
                meta.insert(k, v);
            }
        }
    }
    ToolOutput {
        content: vec![reflect_protocol::ContentBlock::Text { text }],
        is_error: false,
        metadata,
        elapsed_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::ThreadId;
    use reflect_stream::CronScheduler;

    fn tool() -> CronTool {
        CronTool::new(Arc::new(CronScheduler::new(None, ThreadId::new())))
    }

    fn ctx() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn create_list_get_delete_roundtrip() {
        let t = tool();
        // 创建任务
        let out = t
            .execute(
                ctx(),
                serde_json::json!({
                    "action": "create",
                    "schedule": "0 * * * *",
                    "prompt": "standup",
                    "name": "daily"
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let id = out.metadata["id"].as_str().unwrap().to_string();
        assert!(out.metadata["next_fire"].is_string());

        // 列出任务
        let out = t
            .execute(ctx(), serde_json::json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 1);

        // get
        let out = t
            .execute(ctx(), serde_json::json!({"action":"get","id":id}))
            .await
            .unwrap();
        assert_eq!(out.metadata["job"]["prompt"], "standup");

        // 删除任务
        let out = t
            .execute(ctx(), serde_json::json!({"action":"delete","id":id}))
            .await
            .unwrap();
        assert_eq!(out.metadata["removed"], true);
        assert_eq!(out.metadata["action"], "deleted");

        // 列出应为空
        let out = t
            .execute(ctx(), serde_json::json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 0);
    }

    #[tokio::test]
    async fn create_rejects_bad_schedule() {
        let t = tool();
        let err = t
            .execute(
                ctx(),
                serde_json::json!({
                    "action": "create",
                    "schedule": "not-cron",
                    "prompt": "x"
                }),
            )
            .await;
        assert!(err.is_err(), "bad schedule must error");
    }

    #[tokio::test]
    async fn update_disables_job() {
        let t = tool();
        let created = t
            .execute(
                ctx(),
                serde_json::json!({"action":"create","schedule":"0 * * * *","prompt":"p"}),
            )
            .await
            .unwrap();
        let id = created.metadata["id"].as_str().unwrap();
        let out = t
            .execute(
                ctx(),
                serde_json::json!({"action":"update","id":id,"enabled":false}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["job"]["enabled"], false);
    }

    #[tokio::test]
    async fn missing_action_errors() {
        let t = tool();
        let err = t.execute(ctx(), serde_json::json!({})).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn action_permission_routes_mutations_to_prompt() {
        let t = tool();
        // create / delete / update → Prompt(均需审批)
        assert_eq!(
            t.action_permission(&serde_json::json!({"action":"create"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action":"delete"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action":"update"})),
            PermissionMode::Prompt
        );
        // list / get 走 Auto
        assert_eq!(
            t.action_permission(&serde_json::json!({"action":"list"})),
            PermissionMode::Auto
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action":"get"})),
            PermissionMode::Auto
        );
    }

    #[tokio::test]
    async fn delete_unknown_returns_not_found() {
        let t = tool();
        let out = t
            .execute(ctx(), serde_json::json!({"action":"delete","id":"nope"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["removed"], false);
        assert_eq!(out.metadata["action"], "not_found");
    }
}
