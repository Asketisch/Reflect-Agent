//! 工具调用生命周期载荷。

use serde::{Deserialize, Serialize};

use crate::item::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallBeginEvent {
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    /// B1/B2:子 agent 会话 id(嵌套工具调用链追踪)。`None` = 直接工具调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEndEvent {
    pub call_id: String,
    pub output: ToolOutput,
    pub is_error: bool,
    pub elapsed_ms: u64,
    /// B1/B2:与 `ToolCallBeginEvent.child_id` 对应。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
}
