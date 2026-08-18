//! `TaskManager` —— 任务与团队的高层 API。
//!
//! 组合 [`TaskStore`] + [`TeamStore`] + 钩子触发 + event_sink,对外提供
//! 语义化方法(`create_task` / `update_task` / `list_tasks` 等)。Phase 0
//! 只落基础 CRUD,Phase 1 加入 `TaskCreated/Completed/Updated` 钩子触发,
//! Phase 2 加入 `upsert_team` / `delete_team` / `get_team` / `list_teams`,
//! Phase 3 加入 `sync_team_specs` 桥接到 `SubAgentFactory`。
//!
//! ## 钩子集成设计
//!
//! `hook_engine` 与 `event_sink` 都是 `Option<...>`,允许 `TaskManager`
//! 在无钩子场景(headless 测试)下也能工作。Phase 1 把 `reflect-exec`
//! 启动时实例化的 `HookEngine` 注入到 `TaskManager` 里,实现 task
//! 生命周期的对外可见。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use reflect_subagent::{SubAgentFactory, SubAgentSpec};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::error::TaskError;
use crate::model::{ListId, Task, TaskId, TaskStatus, TeamFile};
use crate::store::TaskStore;
use crate::team::validate_team_name;
use crate::team_store::TeamStore;

/// `update_task` 的字段 patch —— 所有字段都 `Option`,调用方只填要改的。
///
/// 区分"清空"和"不改":`subject = Some("")` 表示清空;`subject = None`
/// 表示不改(留给 `TaskUpdate` 工具自己过滤)。metadata / blocks /
/// blocked_by / claimed_by / claimed_at 用单独的 `Patch` 语义(见对应方法)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// 追加阻塞目标(下游 task id)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_blocks: Option<Vec<TaskId>>,
    /// 追加被阻塞(上游 task id)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_blocked_by: Option<Vec<TaskId>>,
    /// 显式设置 / 清空 `claimed_by`(三态:不变 / 改 / 清空)。
    /// `Some(None)` → 清空;`Some(Some("worker@team"))` → 设置;
    /// `None` → 不动。一般由 `TaskClaim` / `TaskRelease` 工具改,
    /// 直接走 `update_task` 的 LLM 调用少用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<Option<String>>,
    /// 显式设置 / 清空 `claimed_at`,与 `claimed_by` 配对更新。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<Option<SystemTime>>,
}

/// `update_task` 的执行结果。
///
/// 结构设计参考(`updatedFields` / `statusChange`),
/// 加上任务对象本身,便于工具实现直接 `Ok(ToolOutput { metadata: ... })`。
#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub task: Task,
    /// 实际改动的字段名(便于工具输出"Updated fields: subject, status")。
    pub updated_fields: Vec<String>,
    /// `(from, to)`,仅在 status 实际变化时为 `Some`。
    pub status_change: Option<(TaskStatus, TaskStatus)>,
}

/// `TaskManager` —— 任务与团队的中央管理器。
pub struct TaskManager {
    pub(crate) task_store: Arc<dyn TaskStore>,
    /// 团队存储后端。`upsert_team` / `delete_team` / `get_team` / `list_teams`
    /// 在 Phase 2 落地,通过本字段访问。
    pub(crate) team_store: Arc<dyn TeamStore>,
    /// 钩子触发器:Phase 1 注入真实 `reflect_hooks::HookEngine`。
    /// Phase 0 保留 `None` 占位,`create_task` / `update_task` 走 no-op 分支。
    pub(crate) hook_engine: Option<Arc<reflect_hooks::HookEngine>>,
    /// 事件 sink,仿 `reflect_discussion::DiscussionOrchestrator::event_sink`。
    /// Phase 0 不发射事件(无消费者);Phase 1 由 `reflect_exec` 注入真实 sink。
    pub(crate) event_sink: Option<Arc<dyn Fn(reflect_protocol::EventMsg) + Send + Sync>>,
    /// `claim_next_available` 用的 per-list 串行化锁。
    ///
    /// 同 list 内多个 worker 并发 claim 时,只有第一个能拿到 `Pending +
    /// !blocked_by` 的任务;后续者等锁释放后再扫一遍(此时第一个已经
    /// 把任务标 `InProgress`,所以会跳过)。锁粒度 = list id,与
    /// `FileTaskStore::list_locks` 同构(`tokio::sync::Mutex` 持有
    /// 可跨 `.await`,正合「load + modify + save」整段原子化)。
    pub(crate) claim_locks: tokio::sync::Mutex<HashMap<ListId, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for TaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskManager")
            .field("task_store", &"Arc<dyn TaskStore>")
            .field("team_store", &"Arc<dyn TeamStore>")
            .field("hook_engine", &self.hook_engine.as_ref().map(|_| "Some"))
            .field("event_sink", &self.event_sink.as_ref().map(|_| "Some"))
            .finish_non_exhaustive()
    }
}

impl TaskManager {
    /// 新建 manager,无钩子无事件 sink。Phase 0 默认形态。
    pub fn new(task_store: Arc<dyn TaskStore>, team_store: Arc<dyn TeamStore>) -> Self {
        Self {
            task_store,
            team_store,
            hook_engine: None,
            event_sink: None,
            claim_locks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 注入钩子引擎。Phase 1 由 `reflect_exec` 启动时调用。
    pub fn with_hook_engine(mut self, engine: Arc<reflect_hooks::HookEngine>) -> Self {
        self.hook_engine = Some(engine);
        self
    }

    /// 注入事件 sink。Phase 1 由 `reflect_exec` 启动时调用,把任务生命周期
    /// 推给 `EventMsg` 消费者(TUI / headless / JSONL 录制器)。
    pub fn with_event_sink(
        mut self,
        sink: Arc<dyn Fn(reflect_protocol::EventMsg) + Send + Sync>,
    ) -> Self {
        self.event_sink = Some(sink);
        self
    }

    // ── 任务增删改查 ───────────────────────────────────────────────────

    /// 创建任务。分配 id,落 store,触发 `TaskCreated` 钩子(success path)。
    pub async fn create_task(
        &self,
        list: &ListId,
        subject: String,
        description: String,
        active_form: Option<String>,
        owner: Option<String>,
        metadata: serde_json::Value,
    ) -> Result<Task, TaskError> {
        let id = self.task_store.next_id(list).await?;
        let now = SystemTime::now();
        // 派生 output_path:文件后端落 `<dir>/<id>.output.md`;in-memory
        // 后端落 `<in-memory>/<list>/<id>.output.md` 占位,TaskOutput 不读。
        let output_path = self
            .task_store
            .dir_for(list)
            .join(format!("{id}.output.md"));
        let task = Task {
            id,
            list_id: list.clone(),
            subject,
            description,
            active_form,
            owner,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata,
            output_path: Some(output_path),
            claimed_by: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        };
        self.task_store.save(list, &task).await?;
        self.fire_task_created(&task).await;
        Ok(task)
    }

    /// 查询任务。`include_deleted = false` 时软删任务返回 `NotFound`。
    pub async fn get_task(
        &self,
        list: &ListId,
        id: TaskId,
        include_deleted: bool,
    ) -> Result<Task, TaskError> {
        let t = self.task_store.load(list, id).await?;
        if !include_deleted && t.status == TaskStatus::Deleted {
            return Err(TaskError::NotFound {
                list: list.clone(),
                id,
            });
        }
        Ok(t)
    }

    /// 更新任务。`patch` 为 `None` 的字段不动;`status=Completed` 时
    /// 触发 `TaskCompleted` 钩子,其他字段变更触发 `TaskUpdated` 钩子。
    pub async fn update_task(
        &self,
        list: &ListId,
        id: TaskId,
        patch: TaskPatch,
    ) -> Result<UpdateOutcome, TaskError> {
        let mut task = self.task_store.load(list, id).await?;
        let mut updated_fields = Vec::new();
        let mut status_change: Option<(TaskStatus, TaskStatus)> = None;

        if let Some(subject) = patch.subject {
            task.subject = subject;
            updated_fields.push("subject".into());
        }
        if let Some(description) = patch.description {
            task.description = description;
            updated_fields.push("description".into());
        }
        if let Some(active_form) = patch.active_form {
            task.active_form = active_form;
            updated_fields.push("active_form".into());
        }
        if let Some(owner) = patch.owner {
            task.owner = owner;
            updated_fields.push("owner".into());
        }
        if let Some(metadata) = patch.metadata {
            task.metadata = metadata;
            updated_fields.push("metadata".into());
        }
        if let Some(extra) = patch.add_blocks {
            for t in extra {
                if !task.blocks.contains(&t) {
                    task.blocks.push(t);
                }
            }
            updated_fields.push("blocks".into());
        }
        if let Some(extra) = patch.add_blocked_by {
            for t in extra {
                if !task.blocked_by.contains(&t) {
                    task.blocked_by.push(t);
                }
            }
            updated_fields.push("blocked_by".into());
        }
        if let Some(claimed_by) = patch.claimed_by {
            task.claimed_by = claimed_by;
            updated_fields.push("claimed_by".into());
        }
        if let Some(claimed_at) = patch.claimed_at {
            task.claimed_at = claimed_at;
            updated_fields.push("claimed_at".into());
        }
        if let Some(new_status) = patch.status
            && new_status != task.status
        {
            status_change = Some((task.status, new_status));
            task.status = new_status;
            updated_fields.push("status".into());
        }

        task.updated_at = SystemTime::now();
        self.task_store.save(list, &task).await?;

        // 钩子触发
        if let Some((_, TaskStatus::Completed)) = status_change {
            self.fire_task_completed(&task).await;
        } else if !updated_fields.is_empty() {
            self.fire_task_updated(&task, &updated_fields).await;
        }

        Ok(UpdateOutcome {
            task,
            updated_fields,
            status_change,
        })
    }

    /// 软删除(把 status 设为 `Deleted`)。Phase 1 工具层 `TaskUpdate status=deleted`
    /// 走 `update_task` 路径;`delete_task` 保留供 CLI / admin 用。
    pub async fn delete_task(&self, list: &ListId, id: TaskId) -> Result<(), TaskError> {
        self.task_store.delete(list, id).await
    }

    /// 原子认领:在该 list 下找第一个 `status == Pending && blocked_by.is_empty()`
    /// 的任务,把它标 `InProgress` 并写入 `claimed_by` / `claimed_at`。
    ///
    /// 并发安全:同 list 内多个 worker 并发调用时,通过 `claim_locks` 串行化
    /// 「find + mutate + save」整段 —— 第一个拿锁的 worker 完成 find 后
    /// 后续者看到的列表里该任务已 `InProgress`,自动跳过。跨 list 完全并行。
    ///
    /// 返回 `Some(task)` 表示认领成功;`None` 表示该 list 下当前没有可认领
    /// 的任务(全部已 `InProgress` / `Completed` / `Deleted`,或上游未完成)。
    pub async fn claim_next_available(
        &self,
        list: &ListId,
        claimer: &str,
    ) -> Result<Option<Task>, TaskError> {
        let lock = self.lock_for_claim(list).await;
        let _guard = lock.lock().await;

        // 在锁内做 find:扫描 list,挑第一个 Pending 且 blocked_by 为空的。
        let tasks = self.task_store.list(list).await?;
        let candidate = tasks
            .into_iter()
            .find(|t| t.status == TaskStatus::Pending && t.blocked_by.is_empty());

        let mut task = match candidate {
            Some(t) => t,
            None => return Ok(None),
        };

        // mutate —— `owner` 也同步成 claimer,这样 LLM 视角的"owner"语义
        // 与协调器协议统一;`status` 推进到 `InProgress`。
        let now = SystemTime::now();
        task.owner = Some(claimer.to_string());
        task.status = TaskStatus::InProgress;
        task.claimed_by = Some(claimer.to_string());
        task.claimed_at = Some(now);
        task.updated_at = now;

        self.task_store.save(list, &task).await?;

        // 钩子触发 —— 把"认领"当成 status 变化(从 Pending → InProgress)
        // 加上 claimed_by 字段填充,复用 `TaskUpdated`。
        let updated_fields = vec![
            "status".into(),
            "owner".into(),
            "claimed_by".into(),
            "claimed_at".into(),
        ];
        self.fire_task_updated(&task, &updated_fields).await;

        Ok(Some(task))
    }

    /// 拿到 list 对应的 claim 锁(同 list 共用,跨 list 独立)。
    async fn lock_for_claim(&self, list: &ListId) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.claim_locks.lock().await;
        map.entry(list.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 列出任务,按 id 升序。`include_deleted = false` 时排除软删。
    pub async fn list_tasks(
        &self,
        list: &ListId,
        _include_deleted: bool,
    ) -> Result<Vec<Task>, TaskError> {
        // store 已自动过滤 `status == Deleted`,include_deleted 参数为
        // 预留扩展点 —— 未来如果 store 增加 "include deleted" 模式可透传。
        self.task_store.list(list).await
    }

    // ── 团队增删改查(Phase 2)───────────────────────────────────

    /// 幂等 upsert:team 已存在则覆盖,成员列表按调用方传入原样替换。
    ///
    /// 校验:
    /// - `name` 必须通过 `validate_team_name`(字符集 + 长度)。
    /// - `lead_agent_id` 必须等于 `team-lead@<name>`(防止 LLM 传错格式)。
    ///
    /// 不触发钩子:team 生命周期通过 `EventMsg::ToolCallEnd { tool_name:
    /// "TeamCreate" }` 透传,Phase 6 TUI 团队胶囊 reducer 直接消费,
    /// 避免给 `HookEventKind` 增加过多低频变体。
    pub async fn upsert_team(&self, team: TeamFile) -> Result<(), TaskError> {
        validate_team_name(&team.name)?;
        let expected_lead = crate::team::lead_agent_id_for(&team.name);
        if team.lead_agent_id != expected_lead {
            return Err(TaskError::Invalid(format!(
                "lead_agent_id '{}' != expected '{expected_lead}'",
                team.lead_agent_id
            )));
        }
        self.team_store.save(&team).await
    }

    /// 物理删除 team 文件(不留 tombstone)。task 文件**不**自动级联
    /// 删除 —— 由 `reflect task purge` 子命令(Phase 3 CLI 落地)统一清理。
    pub async fn delete_team(&self, name: &str) -> Result<(), TaskError> {
        validate_team_name(name)?;
        self.team_store.delete(&name.to_string()).await
    }

    /// 读 team;不存在返回 `TaskError::TeamNotFound`。
    pub async fn get_team(&self, name: &str) -> Result<TeamFile, TaskError> {
        validate_team_name(name)?;
        self.team_store.load(&name.to_string()).await
    }

    /// 列所有 team,按 name 字典序。`list_names` 仅读目录(避免长时间
    /// 阻塞 per-team 锁),再逐个 `load` 拿到完整 `TeamFile`。
    pub async fn list_teams(&self) -> Result<Vec<TeamFile>, TaskError> {
        let mut teams = Vec::new();
        for n in self.team_store.list_names().await? {
            teams.push(self.team_store.load(&n).await?);
        }
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(teams)
    }

    /// 把所有团队的成员 spec 同步到 `SubAgentFactory.dynamic_specs`。
    ///
    /// Phase 3 落地:由 `reflect_exec` 在启动时 / `reflect task team-sync`
    /// CLI 子命令触发。流程:
    /// 1. `list_teams` 拿到所有 `TeamFile`。
    /// 2. 把每个 `TeamMemberSpec` 通过 `From<&TeamMemberSpec> for SubAgentSpec`
    ///    转换为 subagent spec(`agent_id` → `role`,其余字段直传)。
    /// 3. `factory.set_specs(specs)` 整体替换(失败 spec 在 `set_specs` 内 warn 跳过)。
    ///
    /// 返回成功注入的 spec 数量(含 lead 成员;去重以 `role` 为 key)。
    ///
    /// # 设计取舍
    ///
    /// - **整体替换而非增量**:`set_specs` 简单原子;若用户删了 team 但
    ///   `factory` 里残留 spec,下次 sync 自动清理。
    /// - **`name` 冲突以 role 去重**:两个 team 出现相同 role 的成员时,后者
    ///   覆盖前者(`HashMap::insert` 语义);CLI 输出会提示 user 检查命名冲突。
    pub async fn sync_team_specs(&self, factory: &SubAgentFactory) -> Result<usize, TaskError> {
        let teams = self.list_teams().await?;
        let mut specs: Vec<reflect_subagent::SubAgentSpec> = Vec::new();
        for team in &teams {
            for member in &team.members {
                specs.push(SubAgentSpec::from(member));
            }
        }
        let count = specs.len();
        factory.set_specs(specs);
        Ok(count)
    }

    // ── 钩子触发(notification-only:不读 `HookDecision`,只 warn 非 Allow) ──

    async fn fire_task_created(&self, task: &Task) {
        if let Some(engine) = &self.hook_engine {
            let snapshot = serde_json::to_value(task).unwrap_or(serde_json::Value::Null);
            let event = reflect_hooks::HookEvent::TaskCreated { task: snapshot };
            let decision = engine.dispatch(&event).await;
            if !matches!(decision, reflect_hooks::HookDecision::Allow) {
                warn!(?decision, "TaskCreated hook returned non-Allow decision");
            }
        }
    }

    async fn fire_task_completed(&self, task: &Task) {
        if let Some(engine) = &self.hook_engine {
            let snapshot = serde_json::to_value(task).unwrap_or(serde_json::Value::Null);
            let event = reflect_hooks::HookEvent::TaskCompleted {
                task: snapshot,
                previous_status: "in_progress".into(),
            };
            let decision = engine.dispatch(&event).await;
            if !matches!(decision, reflect_hooks::HookDecision::Allow) {
                warn!(?decision, "TaskCompleted hook returned non-Allow decision");
            }
        }
    }

    async fn fire_task_updated(&self, task: &Task, changed: &[String]) {
        if let Some(engine) = &self.hook_engine {
            let snapshot = serde_json::to_value(task).unwrap_or(serde_json::Value::Null);
            let event = reflect_hooks::HookEvent::TaskUpdated {
                task: snapshot,
                changed_fields: changed.to_vec(),
            };
            let decision = engine.dispatch(&event).await;
            if !matches!(decision, reflect_hooks::HookDecision::Allow) {
                warn!(?decision, "TaskUpdated hook returned non-Allow decision");
            }
        }
    }
}

#[cfg(test)]
mod tests;
