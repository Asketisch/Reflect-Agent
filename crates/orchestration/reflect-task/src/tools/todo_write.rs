//! `TodoWrite` — V1 in-memory todo list,与 Task 系统(V2)并存。
//!
//! 接受一个 todos 数组,每个 todo
//! 含 `content` + `status`(`pending` | `in_progress` | `completed`) +
//! `activeForm`(可选)。工具把整个列表序列化到 `metadata.todos`,由
//! `reflect-tui` 的 reducer(Phase 6)在 `EventMsg::ToolCallEnd` 时回填
//! 到 `RenderState.todos` 渲染。
//!
//! **与 V2 的关系**:TodoWrite 不走 `TaskManager`,不持久化,不做跨
//! session 同步。它的存在理由是 LLM 在单次会话里高频更新自己的 working
//! memory(checklist),而 V2 Task 是更正式的多 agent 协调。V1 活在内存里,
//! 退出 session 就消失,与 V2 完全独立。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolOutput};
use reflect_tools::{Tool, ToolContext, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manager::TaskManager;

/// 单条 todo(LLM 写)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoRow {
    pub content: String,
    /// `"pending" | "in_progress" | "completed"` —— 与 `TaskStatus` 同源词,
    /// 但 V1 不引用 `TaskStatus` 枚举以保持独立。
    pub status: String,
    /// `None` 序列化为 JSON `null` —— Phase 6 TUI reducer 检查 `is_null`
    /// 区分"无 active form"vs"字段缺失",显式 null 让 reducer 简单。
    #[serde(default)]
    pub active_form: Option<String>,
}

/// `TodoWrite` 工具实现。
///
/// 注:虽然持有 `TaskManager` 句柄,但当前不调用 —— 留作 Phase 6 之后
/// `TodoWrite → TaskCreate` 自动桥接(用户明确要求"并存"即可,不自动
/// 转化)。`manager` 字段保留以保持 `register_all` 的统一签名。
pub struct TodoWriteTool {
    /// Phase 1 不用,仅占位避免破坏 register_all 签名。
    #[allow(dead_code)]
    manager: Arc<TaskManager>,
}

impl std::fmt::Debug for TodoWriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TodoWriteTool")
            .field("manager", &"Arc<TaskManager>")
            .finish()
    }
}

impl TodoWriteTool {
    pub fn new(manager: Arc<TaskManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn description(&self) -> &str {
        "用给定列表覆盖当前会话的 todo checklist(纯内存,退出会话即失)。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Todo 内容(简短一句话)"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "状态:pending / in_progress / completed(建议同一时刻只留一个 in_progress)"
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "进行时态的简短描述(可选,用于 spinner 文案)"
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    },
                    "description": "完整 todo 列表(整体覆盖,非增量 delta);会覆盖上一轮列表"
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 写 ctx.metadata 是顺序敏感操作;并发写可能被后写覆盖前写。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    async fn execute(&self, _ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let todos = parse_todos(&args)?;

        // Phase 6 之前,工具仅把 todos 通过 metadata 透传给 reducer。
        // 不写 ctx.metadata(那是只读借用);reducer 从
        // `EventMsg::ToolCallEnd { tool_name = "TodoWrite", output.metadata.todos }`
        // 取。
        let count = todos.len();
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(format!(
                "Wrote {count} todos."
            ))],
            is_error: false,
            metadata: serde_json::json!({
                "todos": &todos,
                "count": count,
            }),
            elapsed_ms: 0,
        })
    }
}

/// 从 args 解析 todos 数组,逐项校验 status。
fn parse_todos(args: &Value) -> Result<Vec<TodoRow>, ToolError> {
    let arr = args
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: "missing or non-array 'todos'".into(),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let content = v
            .get("content")
            .and_then(|x| x.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("todos[{i}].content missing or non-string"),
            })?
            .to_string();
        if content.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                message: format!("todos[{i}].content must not be empty or whitespace"),
            });
        }
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("todos[{i}].status missing or non-string"),
            })?
            .to_string();
        if !matches!(status.as_str(), "pending" | "in_progress" | "completed") {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "todos[{i}].status '{status}' invalid; expected pending|in_progress|completed"
                ),
            });
        }
        let active_form = v
            .get("activeForm")
            .and_then(|x| x.as_str())
            .map(String::from);
        out.push(TodoRow {
            content,
            status,
            active_form,
        });
    }
    Ok(out)
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
    async fn writes_todos_with_count() {
        let tool = TodoWriteTool::new(mgr());
        let out = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "todos": [
                        {"content": "design schema", "status": "in_progress", "activeForm": "Designing"},
                        {"content": "write tests", "status": "pending"}
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 2);
        let arr = out.metadata["todos"].as_array().unwrap();
        assert_eq!(arr[0]["content"], "design schema");
        assert_eq!(arr[0]["status"], "in_progress");
        // activeForm 序列化为 camelCase `activeForm`(JSON 默认行为)
        assert_eq!(
            arr[0]["activeForm"], "Designing",
            "got full row: {}",
            arr[0]
        );
        assert_eq!(arr[1]["content"], "write tests");
        // status 字符串保持原样进入 metadata(便于 TUI 直接渲染)
        assert_eq!(arr[1]["status"], "pending");
        // active_form None 序列化为 null,便于 reducer 区分"无"vs"缺失"。
        assert_eq!(arr[1]["activeForm"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn empty_todos_is_allowed() {
        let tool = TodoWriteTool::new(mgr());
        let out = tool
            .execute(ToolContext::default(), serde_json::json!({"todos": []}))
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 0);
    }

    #[tokio::test]
    async fn missing_todos_errors() {
        let tool = TodoWriteTool::new(mgr());
        let err = tool
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn invalid_status_errors() {
        let tool = TodoWriteTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "todos": [{"content": "x", "status": "bogus"}]
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn missing_content_errors() {
        let tool = TodoWriteTool::new(mgr());
        let err = tool
            .execute(
                ToolContext::default(),
                serde_json::json!({
                    "todos": [{"status": "pending"}]
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    /// v1.1.0 review P2-2:content 不能是空 / 纯空白 —— schema `minLength: 1`
    /// 是 LLM 客户端层校验,实际 parse_todos 之前没有运行时检查,LLM strict
    /// 模式下能拦住,但 relaxed 模式会写出空 content,reducer 渲染空 pill。
    /// 修复后 parse_todos 加 `content.trim().is_empty()` 兜底。
    #[tokio::test]
    async fn empty_content_errors() {
        let tool = TodoWriteTool::new(mgr());
        for bad in ["", " ", "\t", "\n  \n"] {
            let err = tool
                .execute(
                    ToolContext::default(),
                    serde_json::json!({"todos": [{"content": bad, "status": "pending"}]}),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidArgs { .. }),
                "empty content '{bad}' should error"
            );
        }
    }

    /// v1.1.0 review P2-1:schema 加 `additionalProperties: false` 让 LLM
    /// strict-mode 拒收未知字段,避免 LLM 误传 `priority` / `id` / 等
    /// 扩展字段导致 reducer 静默丢字段。
    #[test]
    fn schema_rejects_extra_fields() {
        let tool = TodoWriteTool::new(mgr());
        let schema = tool.parameters_schema();
        let items = &schema["properties"]["todos"]["items"];
        assert_eq!(
            items["additionalProperties"], false,
            "todos[*] schema 必须是 closed object"
        );
        assert_eq!(
            schema["additionalProperties"], false,
            "top-level schema 也应是 closed"
        );
    }

    #[test]
    fn metadata_is_stable() {
        let tool = TodoWriteTool::new(mgr());
        assert_eq!(tool.name(), "TodoWrite");
        assert!(!tool.is_concurrency_safe());
        assert_eq!(tool.required_permission(), PermissionMode::Auto);
    }
}
