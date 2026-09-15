//! `background_tasks` —— Background Task Injection(P2 `background-tasks`)。
//!
//! 后台 bash/subagent 任务 stub:spawn 后异步完成,结果在 turn 边界注入。

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
#[derive(Debug, Default)]
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

    /// v1.5 R2:标记任务进入运行态。
    pub fn mark_running(&self, id: &str) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Running;
            return true;
        }
        false
    }

    /// v1.5 R2:标记任务失败并写入原因。
    pub fn fail(&self, id: &str, reason: impl Into<String>) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Failed;
            t.result = Some(reason.into());
            return true;
        }
        false
    }

    /// v1.5 R2:全部任务快照(状态查询工具用;不移除任何条目)。
    pub fn snapshot(&self) -> Vec<BackgroundTask> {
        self.tasks.lock().clone()
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

/// v1.5 R2:单条后台 bash 任务的捕获上限(与 bash 工具一致)。
const BG_OUTPUT_LIMIT: usize = 100 * 1024;

/// v1.5 R2:真实现 —— 后台执行一条 shell 命令,输出(含退出码)在
/// 完成时写回队列,由 turn 边界注入 / `background_status` 工具消费。
/// 进程生命周期绑定会话取消令牌(会话关闭 → kill_on_drop 收割)。
pub fn spawn_bash(
    queue: Arc<BackgroundTaskQueue>,
    session_cancel: CancellationToken,
    cwd: std::path::PathBuf,
    cmd: impl Into<String>,
) -> String {
    use std::process::Stdio;
    let id = format!("bg-{}", uuid_stub_like());
    let cmd = cmd.into();
    queue.register(id.clone(), "bash", cmd.clone());
    let id_for_task = id.clone();
    let q = queue.clone();
    let handle = tokio::spawn(async move {
        let id = id_for_task;
        q.mark_running(&id);
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for key in ["PATH", "HOME", "LANG", "LC_ALL", "USER", "SHELL", "TMPDIR"] {
            if let Ok(v) = std::env::var(key) {
                command.env(key, v);
            }
        }
        let child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                q.fail(&id, format!("spawn failed: {e}"));
                return;
            }
        };
        let mut child = child;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        // 并发排空两端管道(防写满阻塞)+ 等退出;会话取消 → 击杀。
        let collect = async {
            let out_fut = async {
                let mut buf = Vec::new();
                if let Some(s) = stdout.as_mut() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.take(BG_OUTPUT_LIMIT as u64).read_to_end(&mut buf).await;
                }
                buf
            };
            let err_fut = async {
                let mut buf = Vec::new();
                if let Some(s) = stderr.as_mut() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.take(BG_OUTPUT_LIMIT as u64).read_to_end(&mut buf).await;
                }
                buf
            };
            let (out, err) = tokio::join!(out_fut, err_fut);
            let status = child.wait().await;
            (out, err, status)
        };
        tokio::select! {
            biased;
            _ = session_cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                q.fail(&id, "[cancelled] 会话关闭,后台任务被终止");
            }
            r = collect => {
                let (out, err, status) = r;
                let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                let mut text = String::from_utf8_lossy(&out).to_string();
                if !err.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("stderr:\n{}", String::from_utf8_lossy(&err)));
                }
                if text.len() > BG_OUTPUT_LIMIT {
                    text.truncate(BG_OUTPUT_LIMIT);
                    text.push_str("\n... [truncated]");
                }
                text.push_str(&format!("\n[exit_code: {exit_code}]"));
                if exit_code == 0 {
                    q.complete(&id, text);
                } else {
                    q.fail(&id, text);
                }
            }
        }
    });
    // 句柄分离:后台任务生命周期由队列 + 会话令牌管理,drop JoinHandle
    // 即 detach(tokio 语义),不随调用方丢弃。
    drop(handle);
    id
}

fn uuid_stub_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", n)
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

/// v1.5 R2:core 侧的 [`reflect_tools::TaskSpawner`] 实现 —— 桥接工具层
/// 请求与 [`BackgroundTaskQueue`] 的真实进程管理。持有会话取消令牌
/// (后台任务存活期跨 turn,随会话关闭统一收割)。
#[derive(Debug)]
pub struct CoreTaskSpawner {
    queue: Arc<BackgroundTaskQueue>,
    workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
    session_cancel: CancellationToken,
}

impl CoreTaskSpawner {
    pub fn new(
        queue: Arc<BackgroundTaskQueue>,
        workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
        session_cancel: CancellationToken,
    ) -> Self {
        Self {
            queue,
            workspace,
            session_cancel,
        }
    }
}

impl reflect_tools::TaskSpawner for CoreTaskSpawner {
    fn spawn_bash(
        &self,
        cmd: &str,
        cwd: &std::path::Path,
    ) -> Result<String, reflect_protocol::ToolError> {
        // cwd 显式指定优先(工具上下文传当前工作区);否则跟随热切换句柄。
        let cwd = if cwd.as_os_str().is_empty() {
            self.workspace.read().clone()
        } else {
            cwd.to_path_buf()
        };
        Ok(spawn_bash(
            self.queue.clone(),
            self.session_cancel.clone(),
            cwd,
            cmd,
        ))
    }

    fn snapshot(&self) -> Vec<reflect_tools::BackgroundTaskInfo> {
        self.queue
            .snapshot()
            .into_iter()
            .map(|t| reflect_tools::BackgroundTaskInfo {
                id: t.id,
                status: match t.status {
                    BackgroundTaskStatus::Pending => "pending".into(),
                    BackgroundTaskStatus::Running => "running".into(),
                    BackgroundTaskStatus::Completed => "completed".into(),
                    BackgroundTaskStatus::Failed => "failed".into(),
                },
                result: t.result,
            })
            .collect()
    }
}
