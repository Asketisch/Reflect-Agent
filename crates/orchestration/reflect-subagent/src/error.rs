//! 子代理错误。`From<SubAgentError> for ToolError` 放这里,这样
//! `CallSubAgentTool::execute` 失败时,会作为结构化的 `ToolError::Execution` 冒泡上去。

use thiserror::Error;

use reflect_protocol::ToolError;

#[derive(Debug, Error)]
pub enum SubAgentError {
    #[error("subagent nesting depth exceeded (max {max})")]
    MaxDepthExceeded { max: u8 },
    #[error("invalid subagent spec: {0}")]
    SpecInvalid(String),
    #[error("subagent spawn failed: {0}")]
    SpawnFailed(String),
}

impl From<SubAgentError> for ToolError {
    fn from(e: SubAgentError) -> Self {
        ToolError::Execution(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_depth_error_message() {
        let e = SubAgentError::MaxDepthExceeded { max: 3 };
        assert_eq!(e.to_string(), "subagent nesting depth exceeded (max 3)");
    }

    #[test]
    fn converts_to_tool_error() {
        let e: ToolError = SubAgentError::SpecInvalid("bad".into()).into();
        match e {
            ToolError::Execution(s) => assert!(s.contains("invalid subagent spec")),
            _ => panic!("expected Execution variant"),
        }
    }
}
