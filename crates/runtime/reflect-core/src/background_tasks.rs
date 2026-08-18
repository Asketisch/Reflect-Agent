//! `background_tasks` —— Background Task Injection(P2 `background-tasks`)。
//!
//! 后台 bash/subagent 任务 stub:spawn 后异步完成,结果在 turn 边界注入。

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

/// 后台任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

/// 后台任务记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub status: BackgroundTaskStatus,
    pub result: Option<String>,
}

/// 后台任务队列 —— submission_loop 在 turn 边界 drain 已完成项。
#[derive(Default)]
pub struct BackgroundTaskQueue {
    tasks: Mutex<Vec<BackgroundTask>>,
}

impl BackgroundTaskQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 pending 任务。
    pub fn register(
        &self,
        id: impl Into<String>,
        kind: impl Into<String>,
        payload: impl Into<String>,
    ) {
        self.tasks.lock().push(BackgroundTask {
            id: id.into(),
            kind: kind.into(),
            payload: payload.into(),
            status: BackgroundTaskStatus::Pending,
            result: None,
        });
    }

    /// 标记任务完成并写入结果。
    pub fn complete(&self, id: &str, result: impl Into<String>) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Completed;
            t.result = Some(result.into());
            return true;
        }
        false
    }

    /// 取出已完成任务并从队列移除(供注入 LLM context)。
    pub fn drain_completed(&self) -> Vec<BackgroundTask> {
        let mut g = self.tasks.lock();
        let (done, rest): (Vec<_>, Vec<_>) = g
            .drain(..)
            .partition(|t| t.status == BackgroundTaskStatus::Completed);
        *g = rest;
        done
    }

    pub fn len(&self) -> usize {
        self.tasks.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.lock().is_empty()
    }
}

/// spawn 简易后台任务(stub:tokio sleep + 固定结果)。
pub fn spawn_background_stub(
    queue: Arc<BackgroundTaskQueue>,
    id: impl Into<String>,
    kind: impl Into<String>,
    payload: impl Into<String>,
) -> JoinHandle<()> {
    let id_s = id.into();
    queue.register(id_s.clone(), kind, payload);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        queue.complete(&id_s, format!("background result for {id_s}"));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_completed_returns_finished_tasks() {
        let q = Arc::new(BackgroundTaskQueue::new());
        q.register("t1", "bash", "echo hi");
        q.complete("t1", "hi\n");
        let done = q.drain_completed();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].result.as_deref(), Some("hi\n"));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn spawn_stub_completes_task() {
        let q = Arc::new(BackgroundTaskQueue::new());
        let h = spawn_background_stub(q.clone(), "bg-1", "bash", "sleep 0");
        h.await.unwrap();
        let done = q.drain_completed();
        assert_eq!(done.len(), 1);
        assert!(done[0].result.as_ref().unwrap().contains("bg-1"));
    }
}
