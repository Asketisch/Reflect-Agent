//! 模块测试。从原始文件内联的 `#[cfg(test)] mod tests` 迁移而来。

use super::*;
use crate::store::InMemoryTaskStore;
use crate::team_store::InMemoryTeamStore;

fn mgr() -> TaskManager {
    TaskManager::new(
        Arc::new(InMemoryTaskStore::new()),
        Arc::new(InMemoryTeamStore::new()),
    )
}

#[tokio::test]
async fn create_get_update_delete_flow() {
    let m = mgr();
    let t = m
        .create_task(
            &"L".into(),
            "subject".into(),
            "desc".into(),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(t.id, 1);
    assert_eq!(t.status, TaskStatus::Pending);

    let fetched = m.get_task(&"L".into(), 1, false).await.unwrap();
    assert_eq!(fetched.subject, "subject");

    let outcome = m
        .update_task(
            &"L".into(),
            1,
            TaskPatch {
                status: Some(TaskStatus::InProgress),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.status_change,
        Some((TaskStatus::Pending, TaskStatus::InProgress))
    );
    assert!(outcome.updated_fields.contains(&"status".to_string()));

    m.delete_task(&"L".into(), 1).await.unwrap();
    let err = m.get_task(&"L".into(), 1, false).await.unwrap_err();
    assert!(matches!(err, TaskError::NotFound { .. }));
}

#[tokio::test]
async fn ids_increment_per_list() {
    let m = mgr();
    let t1 = m
        .create_task(
            &"L1".into(),
            "a".into(),
            "".into(),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    let t2 = m
        .create_task(
            &"L1".into(),
            "b".into(),
            "".into(),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    let t3 = m
        .create_task(
            &"L2".into(),
            "c".into(),
            "".into(),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(t1.id, 1);
    assert_eq!(t2.id, 2);
    assert_eq!(t3.id, 1);
}

#[tokio::test]
async fn list_filters_soft_deleted() {
    let m = mgr();
    m.create_task(
        &"L".into(),
        "a".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.create_task(
        &"L".into(),
        "b".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    // 把 task 2 软删
    m.update_task(
        &"L".into(),
        2,
        TaskPatch {
            status: Some(TaskStatus::Deleted),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let visible = m.list_tasks(&"L".into(), false).await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, 1);
}

#[tokio::test]
async fn add_blocks_appends_unique() {
    let m = mgr();
    m.create_task(
        &"L".into(),
        "a".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.create_task(
        &"L".into(),
        "b".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.update_task(
        &"L".into(),
        1,
        TaskPatch {
            add_blocks: Some(vec![2]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // 重复追加不增长
    m.update_task(
        &"L".into(),
        1,
        TaskPatch {
            add_blocks: Some(vec![2]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let t = m.get_task(&"L".into(), 1, false).await.unwrap();
    assert_eq!(t.blocks, vec![2]);
}

// ── Phase 2: 团队生命周期 ──

fn make_team(name: &str) -> TeamFile {
    let now = SystemTime::now();
    let lead = crate::team::lead_agent_id_for(name);
    let lead_clone = lead.clone();
    TeamFile {
        name: name.into(),
        description: Some(format!("{name} team")),
        lead_agent_id: lead,
        lead_session_id: None,
        members: vec![crate::model::TeamMemberSpec {
            agent_id: lead_clone,
            name: "team-lead".into(),
            role: "team-lead".into(),
            model: None,
            system_prompt: "lead".into(),
            allowed_tools: vec![],
            color: None,
            joined_at: now,
            session_id: None,
            subscriptions: vec![],
        }],
        created_at: now,
    }
}

#[tokio::test]
async fn upsert_get_team_roundtrip() {
    let m = mgr();
    let t = make_team("rocket");
    m.upsert_team(t.clone()).await.unwrap();
    let back = m.get_team("rocket").await.unwrap();
    assert_eq!(back.name, "rocket");
    assert_eq!(back.lead_agent_id, "team-lead@rocket");
    assert_eq!(back.members.len(), 1);
}

#[tokio::test]
async fn upsert_team_rejects_bad_lead_agent_id() {
    let m = mgr();
    let mut t = make_team("rocket");
    t.lead_agent_id = "wrong-lead@rocket".into();
    let err = m.upsert_team(t).await.unwrap_err();
    assert!(matches!(err, TaskError::Invalid(_)), "got {err:?}");
}

#[tokio::test]
async fn delete_team_happy_then_not_found() {
    let m = mgr();
    m.upsert_team(make_team("rocket")).await.unwrap();
    m.delete_team("rocket").await.unwrap();
    let err = m.get_team("rocket").await.unwrap_err();
    assert!(matches!(err, TaskError::TeamNotFound(_)));
    // 重复 delete 不报错
    m.delete_team("rocket").await.unwrap();
}

#[tokio::test]
async fn list_teams_sorted_by_name() {
    let m = mgr();
    m.upsert_team(make_team("zulu")).await.unwrap();
    m.upsert_team(make_team("alpha")).await.unwrap();
    m.upsert_team(make_team("mike")).await.unwrap();
    let teams = m.list_teams().await.unwrap();
    let names: Vec<&str> = teams.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mike", "zulu"]);
}

#[tokio::test]
async fn list_teams_empty() {
    let m = mgr();
    let teams = m.list_teams().await.unwrap();
    assert!(teams.is_empty());
}

// ── Phase 3:团队 spec 同步(sync_team_specs)──

fn dummy_subagent_factory() -> SubAgentFactory {
    use parking_lot::Mutex;
    use reflect_llm::ModelRegistry;
    use reflect_protocol::ThreadId;
    use reflect_tools::ToolRegistry;
    use tokio_util::sync::CancellationToken;
    let _ = Mutex::new(()); // 保持 Mutex 引用,避开未用导入告警
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

/// 空 manager 同步 → factory 空,返回 0。
#[tokio::test]
async fn sync_team_specs_empty() {
    let m = mgr();
    let factory = dummy_subagent_factory();
    let n = m.sync_team_specs(&factory).await.unwrap();
    assert_eq!(n, 0);
    assert!(factory.list_specs().is_empty());
}

/// 2 个 team × 1 个 lead = 2 个 spec,但 `dynamic_specs` 按 role 去重
/// (两个 `team-lead` 同一 role)→ factory 只存 1 个,返回成功注入
/// 数量 = 2(在调用方看来)而实际 dedupe 由 `set_specs` 完成。
#[tokio::test]
async fn sync_team_specs_converts_members() {
    let m = mgr();
    m.upsert_team(make_team("rocket")).await.unwrap();
    m.upsert_team(make_team("atlas")).await.unwrap();

    let factory = dummy_subagent_factory();
    let n = m.sync_team_specs(&factory).await.unwrap();
    assert_eq!(n, 2, "sync_team_specs 返回传入的 spec 总数");

    // factory dedupe by role:两个 `team-lead` 合一 → 1 个。
    let pairs = factory.list_specs();
    let roles: Vec<&str> = pairs.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roles, vec!["team-lead"]);
}

/// 同步后删除 team,再次 sync → factory spec 清空。
#[tokio::test]
async fn sync_team_specs_replaces_atomically() {
    let m = mgr();
    m.upsert_team(make_team("rocket")).await.unwrap();
    let factory = dummy_subagent_factory();
    m.sync_team_specs(&factory).await.unwrap();
    assert_eq!(factory.list_specs().len(), 1);

    // 删 team 再 sync → factory 清空。
    m.delete_team("rocket").await.unwrap();
    m.sync_team_specs(&factory).await.unwrap();
    assert!(factory.list_specs().is_empty());
}

/// `get_spec(role)` 拿回 SubAgentSpec,字段直传。
#[tokio::test]
async fn sync_team_specs_get_spec_returns_full_spec() {
    let m = mgr();
    m.upsert_team(make_team("rocket")).await.unwrap();
    let factory = dummy_subagent_factory();
    m.sync_team_specs(&factory).await.unwrap();

    let spec = factory
        .get_spec("team-lead")
        .expect("team-lead spec exists");
    assert_eq!(spec.role, "team-lead");
    assert_eq!(spec.name, "team-lead");
    // lead member 的 system_prompt 是 make_team 设的 "lead"
    assert_eq!(spec.system_prompt, "lead");
}

// ── Phase 4: claim_next_available + TaskPatch.claimed_by 三态 ──

/// 正常路径:一个 Pending 任务,worker 认领 → 拿到该任务,
/// status 推进到 `InProgress`,`claimed_by` / `claimed_at` 设值。
#[tokio::test]
async fn claim_next_available_happy_path() {
    let m = mgr();
    m.create_task(
        &"L".into(),
        "alpha".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();

    let claimed = m
        .claim_next_available(&"L".into(), "architect@rocket")
        .await
        .unwrap()
        .expect("should claim a task");
    assert_eq!(claimed.id, 1);
    assert_eq!(claimed.status, TaskStatus::InProgress);
    assert_eq!(claimed.claimed_by.as_deref(), Some("architect@rocket"));
    assert!(claimed.claimed_at.is_some());
    // owner 与 claimed_by 同步
    assert_eq!(claimed.owner.as_deref(), Some("architect@rocket"));

    // 再读一次,持久化生效
    let fetched = m.get_task(&"L".into(), 1, false).await.unwrap();
    assert_eq!(fetched.status, TaskStatus::InProgress);
    assert_eq!(fetched.claimed_by.as_deref(), Some("architect@rocket"));
}

/// 空 list 或无可认领任务 → `Ok(None)`,不抛错。
#[tokio::test]
async fn claim_next_available_returns_none_when_no_pending() {
    let m = mgr();
    // 完全空 list
    let r = m
        .claim_next_available(&"empty".into(), "worker@x")
        .await
        .unwrap();
    assert!(r.is_none());

    // 创建一个已完成的任务,也不该被认领
    m.create_task(
        &"L".into(),
        "done".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.update_task(
        &"L".into(),
        1,
        TaskPatch {
            status: Some(TaskStatus::Completed),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let r = m
        .claim_next_available(&"L".into(), "worker@x")
        .await
        .unwrap();
    assert!(r.is_none());
}

/// `blocked_by` 非空的任务不被认领(上游未完成时跳过)。
#[tokio::test]
async fn claim_next_available_skips_blocked_tasks() {
    let m = mgr();
    // 先创建一个上游任务 1
    m.create_task(
        &"L".into(),
        "upstream".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    // 再创建一个下游任务 2,blocked_by = [1]
    m.create_task(
        &"L".into(),
        "downstream".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.update_task(
        &"L".into(),
        2,
        TaskPatch {
            add_blocked_by: Some(vec![1]),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // 认领应该只拿到上游的 1,而非下游的 2。
    let claimed = m
        .claim_next_available(&"L".into(), "worker@x")
        .await
        .unwrap()
        .expect("should claim upstream");
    assert_eq!(claimed.id, 1);
    assert_eq!(claimed.subject, "upstream");
}

/// 软删(`status == Deleted`)的任务不被认领。
#[tokio::test]
async fn claim_next_available_skips_deleted() {
    let m = mgr();
    m.create_task(
        &"L".into(),
        "deleted".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.create_task(
        &"L".into(),
        "alive".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    m.update_task(
        &"L".into(),
        1,
        TaskPatch {
            status: Some(TaskStatus::Deleted),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let claimed = m
        .claim_next_available(&"L".into(), "worker@x")
        .await
        .unwrap()
        .expect("should claim alive");
    assert_eq!(claimed.id, 2);
    assert_eq!(claimed.subject, "alive");
}

/// 并发安全:4 个 worker 抢 4 个任务,各自拿到不同的 id(无重复 claim)。
#[tokio::test]
async fn claim_next_available_concurrent_no_duplicate() {
    let m = std::sync::Arc::new(mgr());
    for _ in 0..4 {
        m.create_task(
            &"L".into(),
            "t".into(),
            "".into(),
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    }
    let mut joins = Vec::new();
    for i in 0..4 {
        let m = m.clone();
        joins.push(tokio::spawn(async move {
            m.claim_next_available(&"L".into(), &format!("w{i}@t"))
                .await
                .unwrap()
                .map(|t| (t.id, t.claimed_by.unwrap()))
        }));
    }
    let mut results = Vec::new();
    for j in joins {
        results.push(j.await.unwrap());
    }
    // 4 个结果应各有不同的 id,claimer 与 worker 名一致。
    let ids: std::collections::HashSet<u32> = results
        .iter()
        .filter_map(|r| r.as_ref().map(|(id, _)| *id))
        .collect();
    assert_eq!(
        ids.len(),
        4,
        "4 个 worker 应拿到 4 个不同 id,got {results:?}"
    );
    for (id, claimer) in results.iter().flatten() {
        assert!(claimer.starts_with('w'), "claimer 格式: {claimer}");
        assert!(*id >= 1 && *id <= 4);
    }
    // 列表应空 —— 4 个都被认领完。
    let next = m
        .claim_next_available(&"L".into(), "extra@x")
        .await
        .unwrap();
    assert!(next.is_none(), "无剩余可认领");
}

/// `TaskPatch.claimed_by` 三态语义:
/// - `None` → 不动
/// - `Some(Some("..."))` → 设置
/// - `Some(None)` → 清空
#[tokio::test]
async fn update_task_claimed_by_tri_state() {
    let m = mgr();
    m.create_task(
        &"L".into(),
        "x".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();

    // 1) 设置
    let out = m
        .update_task(
            &"L".into(),
            1,
            TaskPatch {
                claimed_by: Some(Some("worker@team".into())),
                claimed_at: Some(Some(SystemTime::UNIX_EPOCH)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.task.claimed_by.as_deref(), Some("worker@team"));
    assert!(out.updated_fields.contains(&"claimed_by".to_string()));

    // 2) None → 不动
    let out = m
        .update_task(
            &"L".into(),
            1,
            TaskPatch {
                subject: Some("new subject".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.task.subject, "new subject");
    assert_eq!(out.task.claimed_by.as_deref(), Some("worker@team"));
    assert!(!out.updated_fields.contains(&"claimed_by".to_string()));

    // 3) Some(None) → 清空
    let out = m
        .update_task(
            &"L".into(),
            1,
            TaskPatch {
                claimed_by: Some(None),
                claimed_at: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(out.task.claimed_by.is_none());
    assert!(out.task.claimed_at.is_none());
}

/// 跨 list 互不干扰:L1 的任务不被 L2 的 claim 路径看到。
#[tokio::test]
async fn claim_next_available_isolated_per_list() {
    let m = mgr();
    m.create_task(
        &"L1".into(),
        "in L1".into(),
        "".into(),
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let claimed = m.claim_next_available(&"L2".into(), "w@x").await.unwrap();
    assert!(claimed.is_none(), "L2 空 list 不该拿到 L1 的任务");
}
