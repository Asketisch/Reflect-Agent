//! 任务系统的统一错误类型。
//!
//! 提供 `From<TaskError> for reflect_tools::ToolError` 转换,允许 TaskManager
//! 在工具实现内 `?` 传播错误,无须手写 `.map_err`。

use thiserror::Error;

use crate::model::{ListId, TaskId};

impl From<TaskError> for reflect_tools::ToolError {
    fn from(e: TaskError) -> Self {
        // "not found" 语义对调用方而言是预期分支(可恢复),走 `Execution`
        // 而非 `InvalidArgs`,让 LLM 把这条消息当正常提示处理。
        match &e {
            TaskError::NotFound { .. } | TaskError::TeamNotFound(_) => {
                reflect_tools::ToolError::Execution(e.to_string())
            }
            _ => reflect_tools::ToolError::Execution(e.to_string()),
        }
    }
}

/// Task / Team 操作失败原因。
#[derive(Debug, Error)]
pub enum TaskError {
    /// 底层 I/O 错误(文件读写、目录创建、原子重命名失败等)。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 序列化/反序列化错误(读写 JSON 失败、字段缺失、类型不匹配等)。
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// 指定 list 下的 task id 不存在。
    #[error("task not found: list={list} id={id}")]
    NotFound { list: ListId, id: TaskId },

    /// 指定名字的 team 不存在。
    #[error("team not found: {0}")]
    TeamNotFound(String),

    /// 用户输入或外部 schema 校验失败(角色不合法、字段缺失、长度越界等)。
    #[error("invalid: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_includes_list_and_id() {
        let e = TaskError::NotFound {
            list: "session-abc".into(),
            id: 42,
        };
        let s = e.to_string();
        assert!(s.contains("session-abc"));
        assert!(s.contains("42"));
    }

    #[test]
    fn team_not_found_includes_name() {
        let e = TaskError::TeamNotFound("rocket".into());
        assert!(e.to_string().contains("rocket"));
    }

    #[test]
    fn invalid_includes_message() {
        let e = TaskError::Invalid("name too long".into());
        assert!(e.to_string().contains("name too long"));
    }
}
