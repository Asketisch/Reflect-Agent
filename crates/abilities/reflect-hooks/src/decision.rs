//! `HookDecision` —— hook 可返回的 5 种决策,加上 `Combined` 包装。
//!
//! 协议层面的含义参见 `docs/tools-and-hooks.md §4.2` / `§4.5`。

use serde::{Deserialize, Serialize};

use reflect_protocol::PermissionMode;

/// hook 可注入到下一次模型调用的系统级提醒。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemMessage {
    pub content: String,
}

impl SystemMessage {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// hook 告知核心要执行的操作。同一事件下不同 hook 返回的多个
/// `HookDecision` 由 [`crate::engine::HookEngine::merge`] 合并。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookDecision {
    /// 允许操作继续执行。
    Allow,
    /// 拒绝操作。对 `PreToolUse` 表示工具不会运行;对 `Stop` 表示
    /// 强制让 turn 继续。
    Deny { reason: String },
    /// 替换工具的参数(仅对 `PreToolUse` 有意义)。
    /// 多个 hook 返回 `ModifyArgs` 时,后者覆盖前者。
    ModifyArgs(serde_json::Value),
    /// 注入系统级提醒(追加到下一次模型调用或工具结果)。
    /// 来自不同 hook 的多个 `InjectMessage` 会拼接。
    InjectMessage(SystemMessage),
    /// 切换即将到来的工具调用所使用的有效权限模式
    /// (仅对 `PreToolUse` 有意义)。
    PermissionOverride(PermissionMode),
    /// 推迟给用户决策:`ToolExecutionQueue` 将该调用路由到
    /// `ApprovalGate::ask_hook`,发出 `EventMsg::ApprovalRequest`
    /// (`ApprovalKind::Hook`)。`reason` 作为决策预览显示在模态框中。
    /// M6。
    Ask { reason: String },
    /// 单次 hook 调用返回的多重决策。
    Combined(Vec<HookDecision>),
}

impl HookDecision {
    /// 展平一层 `Combined`,返回所有叶子决策。
    pub fn flatten(&self) -> Vec<&HookDecision> {
        match self {
            HookDecision::Combined(inner) => inner.iter().collect(),
            other => vec![other],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_serde_roundtrip() {
        let d = HookDecision::Allow;
        let j = serde_json::to_string(&d).unwrap();
        assert_eq!(j, "{\"type\":\"allow\"}");
        let back: HookDecision = serde_json::from_str(&j).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn deny_carries_reason() {
        let d = HookDecision::Deny {
            reason: "no".into(),
        };
        let back: HookDecision = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn ask_carries_reason() {
        let d = HookDecision::Ask {
            reason: "this is risky".into(),
        };
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains(r#""type":"ask""#), "got: {j}");
        let back: HookDecision = serde_json::from_str(&j).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn combined_flattens() {
        let d = HookDecision::Combined(vec![
            HookDecision::Allow,
            HookDecision::Deny { reason: "x".into() },
        ]);
        let leaves = d.flatten();
        assert_eq!(leaves.len(), 2);
    }
}
