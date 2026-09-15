//! 子代理可观测(v1.4 C1)—— 进度推送与状态查询载荷。

use serde::{Deserialize, Serialize};

/// 进度事件类别(协议层镜像,`rename_all` snake_case)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentProgressKind {
    /// 子代理产出了一段完整的助手文本(`AgentMessage`,非增量)。
    Message,
    /// 子代理发起了一个工具调用。
    ToolBegin,
    /// 子代理的一个工具调用结束。
    ToolEnd,
}

/// 通道一(推送):子代理中间进度。父级 `CallSubAgentTool` 在 drain 子
/// 事件流时把 `AgentMessage` / `ToolCallBegin` / `ToolCallEnd` 包装为本
/// 事件发出,客户端(界面 / headless JSONL)实时可见子代理在做什么。
/// 逐字增量(`AgentMessageDelta` / `ThinkingDelta`)不转发,避免刷屏。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentProgressEvent {
    /// 子代理会话号(`SpawnedChild.session_id` 字符串形态)。
    pub child_id: String,
    /// 子代理角色(spec.role,如 `"explorer"`)。
    pub role: String,
    pub kind: SubagentProgressKind,
    /// 文本载荷:Message = 助手文本;ToolBegin = 工具名;ToolEnd =
    /// `工具名`(由调用方从参数侧回填;End 事件本身只带 call_id)。
    pub text: String,
    /// 子代理内部工具调用的 call_id(ToolBegin / ToolEnd 时携带,供客户端
    /// 配对;Message 时省略)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// 子代理运行状态(协议层镜像)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRunStateMirror {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 通道二(查询):单个子代理的状态快照。由引擎侧「子代理状态中心」
/// 在查询时点从共享状态槽克隆 —— 读取方永不阻塞子代理本身。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStatusSnapshot {
    pub child_id: String,
    pub role: String,
    pub state: SubagentRunStateMirror,
    /// 启动时间(RFC3339)。
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// 终态时间;`None` = 仍在运行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 已完成的图迭代次数(model_call 次数,近似「跑了几步」)。
    pub iteration: u32,
    /// 正在执行的工具名;`None` = 当前无工具在跑(或已终态)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    /// 累计 token 用量(总)。
    pub total_tokens: u64,
    /// 最近一条事件摘要(诊断用;截断到 ~200 字符)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
}

/// 通道二(查询)的应答事件:`Op::QuerySubagents` 的回执。`children`
/// 为空表示没有匹配的在飞/近期子代理(或引擎未启用状态中心)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStatusEvent {
    pub children: Vec<SubagentStatusSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进度事件 serde 往返:message 形态省略 call_id。
    #[test]
    fn subagent_progress_serde_roundtrip() {
        let ev = SubagentProgressEvent {
            child_id: "c-1".into(),
            role: "explorer".into(),
            kind: SubagentProgressKind::Message,
            text: "found 3 modules".into(),
            call_id: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("call_id"), "None 的 call_id 应省略");
        let back: SubagentProgressEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.child_id, "c-1");
        assert_eq!(back.role, "explorer");
        assert_eq!(back.kind, SubagentProgressKind::Message);
    }

    /// 状态快照 serde 往返:Running 无 finished_at。
    #[test]
    fn subagent_status_snapshot_serde_roundtrip() {
        let snap = SubagentStatusSnapshot {
            child_id: "c-2".into(),
            role: "writer".into(),
            state: SubagentRunStateMirror::Running,
            started_at: chrono::Utc::now(),
            finished_at: None,
            iteration: 4,
            current_tool: Some("bash".into()),
            total_tokens: 1234,
            last_event: Some("tool begin: bash".into()),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("finished_at"));
        let back: SubagentStatusSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state, SubagentRunStateMirror::Running);
        assert_eq!(back.current_tool.as_deref(), Some("bash"));
        assert_eq!(back.iteration, 4);
    }
}
