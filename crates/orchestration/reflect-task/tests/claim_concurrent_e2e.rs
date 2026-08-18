//! 并发 claim e2e:4 个 worker 同时 `claim_next_available`,各拿不同 task。

use std::sync::Arc;

use reflect_task::{InMemoryTaskStore, InMemoryTeamStore, TaskManager};

fn mgr() -> Arc<TaskManager> {
    Arc::new(TaskManager::new(
        Arc::new(InMemoryTaskStore::new()),
        Arc::new(InMemoryTeamStore::new()),
    ))
}

#[tokio::test]
async fn four_concurrent_claims_get_distinct_tasks() {
    let manager = mgr();
    let list = "session-e2e".to_string();
    for i in 0..4 {
        manager
            .create_task(
                &list,
                format!("task-{i}"),
                String::new(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
    }

    let m = manager.clone();
    let list_c = list.clone();
    let (c0, c1, c2, c3) = tokio::join!(
        async move {
            m.claim_next_available(&list_c, "w0@team")
                .await
                .unwrap()
                .map(|t| (t.id, t.claimed_by.clone()))
        },
        {
            let m = manager.clone();
            let list = list.clone();
            async move {
                m.claim_next_available(&list, "w1@team")
                    .await
                    .unwrap()
                    .map(|t| (t.id, t.claimed_by.clone()))
            }
        },
        {
            let m = manager.clone();
            let list = list.clone();
            async move {
                m.claim_next_available(&list, "w2@team")
                    .await
                    .unwrap()
                    .map(|t| (t.id, t.claimed_by.clone()))
            }
        },
        async move {
            manager
                .claim_next_available(&list, "w3@team")
                .await
                .unwrap()
                .map(|t| (t.id, t.claimed_by.clone()))
        },
    );

    let claims = [c0, c1, c2, c3];
    for c in &claims {
        assert!(c.is_some(), "每个 worker 应 claim 到一项");
    }
    let ids: Vec<_> = claims.iter().map(|c| c.as_ref().unwrap().0).collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        4
    );
}
