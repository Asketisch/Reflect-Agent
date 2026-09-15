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

/// v1.4 A3:工具输出流式增量 —— 长时间运行的工具(bash 构建 / 测试等)
/// 在执行期间逐段上报输出,客户端无需等 `ToolCallEnd` 才能看到进展。
///
/// 与 `ToolCallEndEvent` 的关系:增量是**预览**,最终完整输出(含
/// elapsed_ms / metadata / 脱敏)仍以 End 事件为准;增量不做脱敏(内容
/// 只是进程原样 stdout/stderr 分片,持久化层照常在 End 后脱敏落盘)。
/// `call_id` 与 Begin/End 配对;`is_stderr` 区分流(界面可着色)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallOutputDeltaEvent {
    pub call_id: String,
    /// 本段增量文本(按行或按块,由工具侧决定粒度)。
    pub delta: String,
    /// 本段是否来自标准错误流。
    #[serde(default)]
    pub is_stderr: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1.4 A3:增量事件 serde 往返 —— `is_stderr` 缺省反序列化为 false
    /// (旧 payload / 简化写入方兼容)。
    #[test]
    fn tool_call_output_delta_serde_roundtrip() {
        let ev = ToolCallOutputDeltaEvent {
            call_id: "call-42".into(),
            delta: "compiling foo...\n".into(),
            is_stderr: true,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ToolCallOutputDeltaEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.call_id, "call-42");
        assert!(back.is_stderr);
        assert_eq!(back.delta, "compiling foo...\n");
    }

    /// `is_stderr` 字段缺省 → false(#[serde(default)] 兼容性)。
    #[test]
    fn tool_call_output_delta_is_stderr_defaults_false() {
        let back: ToolCallOutputDeltaEvent =
            serde_json::from_str(r#"{"call_id":"c","delta":"d"}"#).unwrap();
        assert!(!back.is_stderr);
    }
}
