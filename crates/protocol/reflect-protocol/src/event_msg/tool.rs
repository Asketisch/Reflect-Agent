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

/// 流式工具参数增量(v1.5 additive 变体,Protocol v0 冻结规则下新增 = 兼容)。
///
/// 模型生成 tool_use 参数期间,把 LLM 层的 `ChatEvent::ToolUseDelta`
/// 原样转发给客户端:call_id 由首个未终结的 ToolUseStart 提供。客户端
/// (TUI / SDK)可在**模型仍在生成时**渐进渲染 write / edit 等长参数
/// 工具的输入,不再面对 `ToolCallBegin(args 恒 null)` 的黑盒等待。
///
/// 兼容策略:`#[serde(default)]` 的字段形态与其它事件一致;老客户端
/// 收到未知 `type` 按 PROTOCOL.md 约定忽略即可。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallArgsDeltaEvent {
    /// 进行中的 tool_use 调用标识(与后续 `ToolCallBegin` 的工具对应)。
    pub call_id: String,
    /// 本次增量追加的原始 JSON 参数片段(未解析,由消费方累积)。
    pub delta: String,
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

/// v1.3 SDK:core → 客户端(serve 模式)的**远程工具执行请求**。
///
/// 客户端通过 `Op::RegisterTools` 注册的自定义工具被 LLM 调用时,
/// core 侧 `RemoteTool` 发出本事件;客户端在本地执行 handler 后用
/// `Op::ToolExecutionResponse { call_id, output }` 回执(同一 `call_id`
/// 配对)。等待语义与 `ApprovalRequest` 同构:超时 / 取消 / 通道关闭
/// 均会让工具调用以错误收尾。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequestEvent {
    /// 与 `Op::ToolExecutionResponse.call_id` 配对的请求标识(uuid)。
    pub call_id: String,
    /// 工具名(与 `Op::RegisterTools` 注册的 `RemoteToolSpec.name` 一致)。
    pub tool: String,
    /// LLM 发起的调用参数(已解析的 JSON 对象)。
    pub args: serde_json::Value,
}
