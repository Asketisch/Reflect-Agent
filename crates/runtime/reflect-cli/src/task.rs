//! `reflect task ...` 与 `reflect task team ...` —— CLI 直连 TaskManager。
//!
//! v1.1.0 落地。CLI 不经过 `Tool` execute 路径,直接构造 `TaskManager`
//! (FileTaskStore + FileTeamStore) 调对应方法,返回结构化输出。
//!
//! 持久化路径:
//! - 任务: `~/.reflect/tasks/<list_id>/<task_id>.json`
//! - 团队: `~/.reflect/teams/<name>.json`
//!
//! 与 `reflect session ...` 的设计对称 —— session 走 `reflect-rollout`
//! 的 JSONL 扫描,task/team 走 `reflect-task::TaskManager` 文件后端。

use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, anyhow};
use reflect_task::model::{Task, TaskStatus, TeamFile};
use reflect_task::{FileTaskStore, FileTeamStore, TaskError, TaskManager};

/// 读 `$HOME`,作为 `~/.reflect` 的根(`FileTaskStore::with_default_home`
/// 内部会把 `$HOME/.reflect` 当 home,所以我们直接用 `with_default_home` 即可)。
fn build_manager() -> anyhow::Result<TaskManager> {
    let task_store =
        FileTaskStore::with_default_home().map_err(|e| anyhow!("FileTaskStore init: {e}"))?;
    let team_store =
        FileTeamStore::with_default_home().map_err(|e| anyhow!("FileTeamStore init: {e}"))?;
    Ok(TaskManager::new(Arc::new(task_store), Arc::new(team_store)))
}

/// `~/.reflect` 根目录 —— 任务 / 团队文件都落这里。
///
/// 镜像 `FileTaskStore::with_default_home` 的 fallback 顺序:优先
/// `$REFLECT_HOME`,否则 `$HOME/.reflect`。
fn reflect_home() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("REFLECT_HOME") {
        return Ok(std::path::PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME unset"))?;
    Ok(std::path::PathBuf::from(home).join(".reflect"))
}

/// `reflect task ls [--list <id>] [--include-deleted]` —— 列任务。
///
/// 默认 `--list` = 当前 session ID(`reflect-task` V2 约定 worker 视角)。
/// 不指定 list 时扫描 `~/.reflect/tasks/` 下所有子目录汇总展示。
pub async fn ls(list: Option<&str>, include_deleted: bool) -> anyhow::Result<()> {
    let m = build_manager()?;
    let lists = if let Some(l) = list {
        vec![l.to_string()]
    } else {
        // 不指定 list → 扫所有子目录。
        discover_lists(&m)?
    };
    if lists.is_empty() {
        println!("(no task lists under ~/.reflect/tasks/)");
        return Ok(());
    }
    println!(
        "{:<6}  {:<40}  {:<14}  {:<14}  list",
        "id", "subject", "status", "owner"
    );
    let mut any = false;
    for l in &lists {
        let tasks = m
            .list_tasks(&l.clone(), include_deleted)
            .await
            .with_context(|| format!("list_tasks({l})"))?;
        for t in tasks {
            any = true;
            println!(
                "{:<6}  {:<40}  {:<14}  {:<14}  {}",
                t.id,
                truncate(&t.subject, 40),
                t.status.to_string(),
                t.owner.as_deref().unwrap_or("-"),
                t.list_id,
            );
        }
    }
    if !any {
        println!("(no tasks found)");
    }
    Ok(())
}

/// `reflect task show --list <id> <task_id>` —— 显示单条任务详情。
pub async fn show(list: &str, task_id: u32) -> anyhow::Result<()> {
    let m = build_manager()?;
    let t = m
        .get_task(&list.into(), task_id, true)
        .await
        .map_err(|e| match e {
            TaskError::NotFound { .. } => anyhow!("task not found: list={list} id={task_id}"),
            other => anyhow!("get_task failed: {other}"),
        })?;
    print_task_detail(&t);
    Ok(())
}

/// `reflect task create --subject <s> [--description <d>] [--list <id>] [--active-form <a>]` —— 建任务。
pub async fn create(
    subject: &str,
    description: &str,
    list: &str,
    active_form: Option<&str>,
) -> anyhow::Result<()> {
    if subject.trim().is_empty() {
        return Err(anyhow!("--subject cannot be empty"));
    }
    let m = build_manager()?;
    let t = m
        .create_task(
            &list.into(),
            subject.to_string(),
            description.to_string(),
            active_form.map(String::from),
            None,
            serde_json::json!({}),
        )
        .await
        .map_err(|e| anyhow!("create_task failed: {e}"))?;
    println!(
        "Task #{} created in list '{}': {}",
        t.id, t.list_id, t.subject
    );
    Ok(())
}

/// `reflect task update <id> [--status <s>] [--subject <s>] [--list <id>]` —— 改任务。
pub async fn update(
    task_id: u32,
    list: &str,
    status: Option<&str>,
    subject: Option<&str>,
) -> anyhow::Result<()> {
    let mut patch = reflect_task::TaskPatch::default();
    if let Some(s) = status {
        let parsed = parse_status(s)?;
        patch.status = Some(parsed);
    }
    if let Some(s) = subject {
        if s.trim().is_empty() {
            return Err(anyhow!("--subject cannot be empty"));
        }
        patch.subject = Some(s.to_string());
    }
    if patch.status.is_none() && patch.subject.is_none() {
        return Err(anyhow!("no-op update; pass --status or --subject"));
    }
    let m = build_manager()?;
    let outcome = m
        .update_task(&list.into(), task_id, patch)
        .await
        .map_err(|e| match e {
            TaskError::NotFound { .. } => {
                anyhow!("task not found: list={list} id={task_id}")
            }
            other => anyhow!("update_task failed: {other}"),
        })?;
    println!(
        "Task #{} updated. Fields: {}",
        outcome.task.id,
        if outcome.updated_fields.is_empty() {
            "(none)".to_string()
        } else {
            outcome.updated_fields.join(", ")
        }
    );
    if let Some((from, to)) = outcome.status_change {
        println!("  status: {from} → {to}");
    }
    Ok(())
}

/// `reflect task stop <id> --list <id>` —— 物理删除 task 文件。
///
/// 与 `TaskUpdate status=deleted` 的语义不同:本命令直接调 `delete_task`,
/// 文件从磁盘消失,不可恢复。TaskUpdate 走 `update_task` 软删。
pub async fn stop(list: &str, task_id: u32) -> anyhow::Result<()> {
    let m = build_manager()?;
    m.delete_task(&list.into(), task_id)
        .await
        .map_err(|e| match e {
            TaskError::NotFound { .. } => {
                anyhow!("task not found: list={list} id={task_id}")
            }
            other => anyhow!("delete_task failed: {other}"),
        })?;
    println!("Task #{task_id} removed from list '{list}'.");
    Ok(())
}

/// `reflect task purge [--team <name>] [--list <id>] [--yes]` —— 级联清理 task 文件。
///
/// 用途:`TeamDelete` 不级联删 task,留待本命令显式清理。`--team` 模式
/// 删除 `<home>/tasks/<team-name>/` 整个目录;`--list` 模式等价
/// `--team`(CLI 上是同一个语义);不传则删除整个 `<home>/tasks/`。
pub fn purge(team: Option<&str>, list: Option<&str>, yes: bool) -> anyhow::Result<()> {
    let home = reflect_home()?;
    let target = match team.or(list) {
        Some(name) => home.join("tasks").join(name),
        None => home.join("tasks"),
    };
    if !target.exists() {
        println!("(nothing to purge: {} does not exist)", target.display());
        return Ok(());
    }
    if !yes {
        eprint!("purge {}? [y/N] ", target.display());
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }
    let meta =
        std::fs::symlink_metadata(&target).with_context(|| format!("stat {}", target.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(&target)
            .with_context(|| format!("remove_dir_all {}", target.display()))?;
        println!("purged directory {}", target.display());
    } else {
        std::fs::remove_file(&target)
            .with_context(|| format!("remove_file {}", target.display()))?;
        println!("purged file {}", target.display());
    }
    Ok(())
}

// ── team 子命令 ────────────────────────────────────────────────────

/// 构造 CLI 用 `SubAgentFactory`(无 recorder,与 pipeline CLI 对称)。
fn build_subagent_factory() -> reflect_subagent::SubAgentFactory {
    use reflect_llm::ModelRegistry;
    use reflect_tools::ToolRegistry;
    use tokio_util::sync::CancellationToken;

    let registry = Arc::new(ModelRegistry::new());
    let parent_tools = Arc::new(ToolRegistry::default());
    reflect_subagent::SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        String::from("openai/gpt-4o"),
        registry,
        None, // child_registry: 回退父级 registry
        parent_tools,
        CancellationToken::new(),
        None,
    )
}

/// `reflect task team sync [--list-specs]` —— 把 team 成员 spec 同步到 factory 并可选列出。
pub async fn team_sync(list_specs_only: bool) -> anyhow::Result<()> {
    let m = build_manager()?;
    let factory = build_subagent_factory();
    let count = m
        .sync_team_specs(&factory)
        .await
        .map_err(|e| anyhow!("sync_team_specs failed: {e}"))?;

    if list_specs_only {
        let pairs = factory.list_specs();
        if pairs.is_empty() {
            println!("(no dynamic specs; sync injected {count} member spec(s) from teams)");
            return Ok(());
        }
        println!("{:<24}  name", "role");
        for (role, name) in pairs {
            println!("{role:<24}  {name}");
        }
        return Ok(());
    }

    println!("Synced {count} team member spec(s) into SubAgentFactory.dynamic_specs.");
    let pairs = factory.list_specs();
    if !pairs.is_empty() {
        println!("Active specs ({}):", pairs.len());
        for (role, name) in pairs {
            println!("  {role} → {name}");
        }
    }
    Ok(())
}

/// `reflect task team ls` —— 列出所有 team(按 name 字典序)。
pub async fn team_ls() -> anyhow::Result<()> {
    let m = build_manager()?;
    let teams = m.list_teams().await.context("list_teams")?;
    if teams.is_empty() {
        println!("(no teams under ~/.reflect/teams/)");
        return Ok(());
    }
    println!("{:<24}  {:<14}  {:<8}  members", "name", "lead", "members");
    for t in &teams {
        println!(
            "{:<24}  {:<14}  {:<8}  {}",
            t.name,
            t.lead_agent_id,
            t.members.len(),
            t.members
                .iter()
                .map(|m| m.agent_id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    Ok(())
}

/// `reflect task team show <name>` —— 显示单个 team 详情。
pub async fn team_show(name: &str) -> anyhow::Result<()> {
    let m = build_manager()?;
    let t = m.get_team(name).await.map_err(|e| match e {
        TaskError::TeamNotFound(_) => anyhow!("team '{name}' not found"),
        other => anyhow!("get_team failed: {other}"),
    })?;
    print_team_detail(&t);
    Ok(())
}

/// `reflect task team create <name> [--description <d>]` —— 建 team(只含 lead)。
pub async fn team_create(name: &str, description: Option<&str>) -> anyhow::Result<()> {
    let lead = reflect_task::lead_agent_id_for(name);
    let now = SystemTime::now();
    let t = TeamFile {
        name: name.into(),
        description: description.map(String::from),
        lead_agent_id: lead,
        lead_session_id: None,
        members: vec![reflect_task::model::TeamMemberSpec {
            agent_id: format!("team-lead@{name}"),
            name: "team-lead".into(),
            role: "team-lead".into(),
            model: None,
            system_prompt:
                "You are the team lead. Coordinate team members and own the team-level task list."
                    .into(),
            allowed_tools: vec![],
            color: None,
            joined_at: now,
            session_id: None,
            subscriptions: vec![],
        }],
        created_at: now,
    };
    let m = build_manager()?;
    m.upsert_team(t.clone())
        .await
        .map_err(|e| anyhow!("upsert_team failed: {e}"))?;
    println!(
        "Team '{}' created ({} members, lead: {}).",
        t.name,
        t.members.len(),
        t.lead_agent_id
    );
    Ok(())
}

/// `reflect task team delete <name>` —— 物理删除 team 文件(不级联)。
pub async fn team_delete(name: &str, yes: bool) -> anyhow::Result<()> {
    let m = build_manager()?;
    let prior = m.get_team(name).await.ok();
    if prior.is_none() {
        return Err(anyhow!("team '{name}' not found"));
    }
    if !yes {
        eprint!(
            "delete team '{name}' ({} members)? [y/N] ",
            prior.as_ref().unwrap().members.len()
        );
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }
    m.delete_team(name)
        .await
        .map_err(|e| anyhow!("delete_team failed: {e}"))?;
    println!(
        "Team '{}' deleted (was {} members; tasks not removed — use `reflect task purge --team {}`).",
        name,
        prior.as_ref().unwrap().members.len(),
        name
    );
    Ok(())
}

// ── 辅助函数 ───────────────────────────────────────────────────────────

/// 走 `~/.reflect/tasks/` 目录树,收集所有 list 名称(字典序)。
fn discover_lists(m: &TaskManager) -> anyhow::Result<Vec<String>> {
    let base = reflect_home()?.join("tasks");
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&base).with_context(|| format!("read_dir {}", base.display()))? {
        let entry = entry?;
        let ft = entry.file_type().ok();
        if !ft.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 跳过 `.highwatermark` / `.lock` 等 dot 文件/隐藏子目录。
        if name.starts_with('.') {
            continue;
        }
        out.push(name);
    }
    out.sort();
    let _ = m; // suppress unused warning; 预留 future TaskStore::list_lists
    Ok(out)
}

/// 把 `TaskStatus` 字符串解析成 enum。CLI 接受 kebab / snake 两种。
fn parse_status(s: &str) -> anyhow::Result<TaskStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(TaskStatus::Pending),
        "in_progress" | "in-progress" | "inprogress" => Ok(TaskStatus::InProgress),
        "completed" | "done" => Ok(TaskStatus::Completed),
        "deleted" => Ok(TaskStatus::Deleted),
        other => Err(anyhow!(
            "unknown status '{other}'; accepted: pending | in_progress | completed | deleted"
        )),
    }
}

fn print_task_detail(t: &Task) {
    println!("Task #{}  (list: {})", t.id, t.list_id);
    println!("  subject      : {}", t.subject);
    println!("  description  : {}", t.description);
    println!("  status       : {}", t.status);
    println!(
        "  owner        : {}",
        t.owner.as_deref().unwrap_or("(unassigned)")
    );
    println!(
        "  active_form  : {}",
        t.active_form.as_deref().unwrap_or("(none)")
    );
    println!("  blocks       : {:?}", t.blocks);
    println!("  blocked_by   : {:?}", t.blocked_by);
    if let Some(p) = &t.output_path {
        println!("  output_path  : {}", p.display());
    }
    println!(
        "  created_at   : {:?}",
        t.created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    println!(
        "  updated_at   : {:?}",
        t.updated_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
}

fn print_team_detail(t: &TeamFile) {
    println!("Team '{}'", t.name);
    println!(
        "  description  : {}",
        t.description.as_deref().unwrap_or("(none)")
    );
    println!("  lead         : {}", t.lead_agent_id);
    println!("  members      : {}", t.members.len());
    for (i, m) in t.members.iter().enumerate() {
        println!(
            "    [{}] agent_id={:<24}  role={:<14}  model={}",
            i,
            m.agent_id,
            m.role,
            m.model.as_deref().unwrap_or("(default)"),
        );
    }
    println!(
        "  created_at   : {:?}",
        t.created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
}

/// UTF-8 字符级截断到 `max_chars`,超出追加 `…`。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count >= max_chars.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
    }
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_home::lock_home;
    use std::fs;
    use tempfile::TempDir;

    /// `parse_status` 接受 4 个合法值 + kebab 别名。
    #[test]
    fn parse_status_accepts_legal_values() {
        assert!(matches!(
            parse_status("pending").unwrap(),
            TaskStatus::Pending
        ));
        assert!(matches!(
            parse_status("in_progress").unwrap(),
            TaskStatus::InProgress
        ));
        assert!(matches!(
            parse_status("in-progress").unwrap(),
            TaskStatus::InProgress
        ));
        assert!(matches!(
            parse_status("completed").unwrap(),
            TaskStatus::Completed
        ));
        assert!(matches!(
            parse_status("done").unwrap(),
            TaskStatus::Completed
        ));
        assert!(matches!(
            parse_status("deleted").unwrap(),
            TaskStatus::Deleted
        ));
    }

    /// `parse_status` 拒绝非法值 + 报错带 hint。
    #[test]
    fn parse_status_rejects_unknown() {
        let err = parse_status("garbage").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown status"), "got: {msg}");
        assert!(msg.contains("pending"), "got: {msg}");
    }

    /// `truncate` 边界正确(`max_chars` 自身 = `…` 占位)。
    #[test]
    fn truncate_helper_at_boundary() {
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abcdefgh", 4), "abc…");
        assert_eq!(truncate("", 4), "");
    }

    /// 端到端:create → show → update → ls → stop,走真 IO + tmpdir HOME。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn task_crud_round_trip_via_real_io() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        // 创建第一个任务
        create("write tests", "add unit tests for store", "session-x", None)
            .await
            .unwrap();
        // 创建第二个任务
        create("fix bug", "in TaskManager", "session-x", Some("Fixing"))
            .await
            .unwrap();

        // 查看任务
        show("session-x", 1).await.unwrap();
        // ls (默认 list = 单一目录扫描)
        ls(Some("session-x"), false).await.unwrap();

        // 更新状态为 in_progress
        update(1, "session-x", Some("in_progress"), None)
            .await
            .unwrap();

        // stop (物理删除)
        stop("session-x", 2).await.unwrap();

        // ls 应该只剩 task #1
        // 通过底层 manager 验证(避免再调一次 ls 打印干扰)。
        let m = build_manager().unwrap();
        let tasks = m.list_tasks(&"session-x".into(), false).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].status, TaskStatus::InProgress);
    }

    /// 端到端:team create → show → ls → delete。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn team_crud_round_trip_via_real_io() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        team_create("rocket", Some("Build rocket")).await.unwrap();
        team_create("atlas", None).await.unwrap();

        team_show("rocket").await.unwrap();
        team_ls().await.unwrap();

        team_delete("rocket", true).await.unwrap();
        // 二次 delete 应报错
        let err = team_delete("rocket", true).await.unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    /// `purge --team <name>` 删除 list 目录;`--yes` 跳过确认。
    #[test]
    fn purge_team_removes_list_dir() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        // `reflect_home()` = `$HOME/.reflect`,所以 task 目录在 `.reflect/tasks/`。
        let team_dir = home.path().join(".reflect").join("tasks").join("rocket");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(team_dir.join("1.json"), "{}").unwrap();
        assert!(team_dir.exists());

        purge(Some("rocket"), None, true).unwrap();
        assert!(!team_dir.exists());
    }

    /// `purge` 不存在的路径 → no-op。
    #[test]
    fn purge_missing_path_is_noop() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        purge(Some("ghost"), None, true).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn team_sync_lists_specs_after_create() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        team_create("sync-team", None).await.unwrap();
        team_sync(true).await.unwrap();
    }

    /// `team_create` 拒绝非法名(大写)。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn team_create_rejects_uppercase_name() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        let err = team_create("BadName", None).await.unwrap_err();
        assert!(format!("{err}").contains("name must match"));
    }

    /// `create` 拒绝空 subject。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn task_create_rejects_empty_subject() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        let err = create("", "x", "L", None).await.unwrap_err();
        assert!(format!("{err}").contains("subject"));
    }

    /// `update` 不传 --status / --subject 报错。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn task_update_requires_at_least_one_field() {
        let home = TempDir::new().unwrap();
        let _guard = lock_home(home.path());

        let err = update(1, "L", None, None).await.unwrap_err();
        assert!(format!("{err}").contains("no-op update"));
    }
}
