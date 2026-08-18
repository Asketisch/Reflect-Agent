//! `TaskOutput` — 读任务的输出文件 (`task.output_path`)。
//!
//! 核心契约:
//! - `block = true`(默认)+ `timeoutMs = 30000`(默认):轮询文件直到出现
//!   或超时。轮询间隔 100ms,模仿 Unix `tail -f` 风格。
//! - `block = false`:立即读,文件不存在返回空串(`""`),不报错。
//!
//! 文件由 `TaskCreate` 自动分配:`<dir>/<id>.output.md`(`<dir>` 来自
//! `TaskStore::dir_for`)。Owner agent(执行任务的 worker)负责把过程
//! 写入此文件,TUI/下游 agent 用此工具读回。
//!
//! `required_permission = Auto` —— 这是只读操作,不修改任何状态。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde_json::Value;
use tokio::time::sleep;
use tracing::warn;

use crate::manager::TaskManager;
use crate::tools::{list_id_from_args_or_ctx, parse_u32_arg};

/// `TaskOutput` 工具实现。
pub struct TaskOutputTool {
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TaskOutputTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskOutputTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TaskOutputTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

/// 轮询文件出现,直到文件存在或超时。100ms 间隔。
async fn wait_for_file(path: &PathBuf, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::fs::read_to_string(path).await {
            Ok(s) => return Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                warn!(path = ?path, error = %e, "TaskOutput read error");
                return None;
            }
        }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        "读取任务的输出文件(task.output_path,TaskCreate 自动分配)。\
         block=true(默认)等文件出现或 timeoutMs(默认 30000)超时;\
         block=false 立即读,文件不存在返回空串。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "integer", "minimum": 1, "description": "Task id to read output for"},
                "listId": {"type": "string", "description": "List id; defaults to current session id"},
                "block": {
                    "type": "boolean",
                    "default": true,
                    "description": "Wait for output file to appear if missing"
                },
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 30000,
                    "description": "Maximum wait time in milliseconds when block=true"
                }
            },
            "required": ["taskId"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 只读,多个并发 TaskOutput 调互不影响。
        true
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let list_id = list_id_from_args_or_ctx(&args, &ctx);
        let task_id = parse_u32_arg(&args, "taskId")?;
        let block = args.get("block").and_then(|v| v.as_bool()).unwrap_or(true);
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        let task = self.manager.get_task(&list_id, task_id, false).await?;
        let path = task.output_path.clone().ok_or_else(|| {
            ToolError::Execution(format!(
                "task #{task_id} has no output_path assigned; TaskCreate should allocate one"
            ))
        })?;

        let content = if block {
            // block=true:等待文件出现,超时返回空串(不报错,跟 Unix tail 语义一致)
            wait_for_file(&path, Duration::from_millis(timeout_ms))
                .await
                .unwrap_or_default()
        } else {
            // block=false:立即读,文件不存在返回空串
            tokio::fs::read_to_string(&path).await.unwrap_or_default()
        };

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(content.clone())],
            is_error: false,
            metadata: serde_json::json!({
                "taskId": task.id,
                "listId": task.list_id,
                "path": path,
                "block": block,
                "timeoutMs": timeout_ms,
                "contentLength": content.len(),
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileTaskStore, InMemoryTaskStore, InMemoryTeamStore};
    use reflect_tools::Tool;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reads_existing_output_file() {
        let dir = TempDir::new().unwrap();
        let m = Arc::new(TaskManager::new(
            Arc::new(FileTaskStore::new(dir.path())),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let t = m
            .create_task(
                &"L".into(),
                "x".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        // 模拟 owner agent 写入 output
        let path = t.output_path.as_ref().unwrap();
        tokio::fs::write(path, b"hello world").await.unwrap();

        let tool = TaskOutputTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": t.id, "listId": "L", "block": false}),
            )
            .await
            .unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text"),
        }
        assert_eq!(out.metadata["contentLength"], 11);
    }

    #[tokio::test]
    async fn block_false_returns_empty_when_missing() {
        let m = Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let t = m
            .create_task(
                &"L".into(),
                "x".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let tool = TaskOutputTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": t.id, "listId": "L", "block": false}),
            )
            .await
            .unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => assert_eq!(text, ""),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn block_true_returns_after_file_appears() {
        let dir = TempDir::new().unwrap();
        let m = Arc::new(TaskManager::new(
            Arc::new(FileTaskStore::new(dir.path())),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let t = m
            .create_task(
                &"L".into(),
                "x".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let path = t.output_path.as_ref().unwrap().clone();
        let tool = TaskOutputTool::new(m);
        let path_for_writer = path.clone();
        // 200ms 后另一个 task 写入
        let writer = tokio::spawn(async move {
            sleep(Duration::from_millis(200)).await;
            tokio::fs::write(&path_for_writer, b"delayed")
                .await
                .unwrap();
        });
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "taskId": t.id,
                    "listId": "L",
                    "block": true,
                    "timeoutMs": 2000
                }),
            )
            .await
            .unwrap();
        writer.await.unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => assert_eq!(text, "delayed"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn block_true_returns_empty_on_timeout() {
        let m = Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let t = m
            .create_task(
                &"L".into(),
                "x".into(),
                "".into(),
                None,
                None,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let tool = TaskOutputTool::new(m);
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "taskId": t.id,
                    "listId": "L",
                    "block": true,
                    "timeoutMs": 200
                }),
            )
            .await
            .unwrap();
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => assert_eq!(text, ""),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn missing_task_errors() {
        let tool = TaskOutputTool::new(Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        )));
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({"taskId": 999, "listId": "L"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn metadata_is_stable() {
        let m = Arc::new(TaskManager::new(
            Arc::new(InMemoryTaskStore::new()),
            Arc::new(InMemoryTeamStore::new()),
        ));
        let tool = TaskOutputTool::new(m);
        assert_eq!(tool.name(), "TaskOutput");
        assert!(tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
    }
}
