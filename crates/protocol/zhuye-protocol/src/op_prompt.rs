//! v1.6.1:提示词层管理(`/prompt` slash)的协议载荷。
//!
//! `Op::PromptCommand` 的 action 在此定义;应答事件体 `PromptListEvent` /
//! `PromptSwitchedEvent` 定义在 [`crate::event_msg::config`](同 EventMsg
//! 家族一处),经 crate 根 re-export。

use serde::{Deserialize, Serialize};

/// `/prompt` 子命令动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptAction {
    /// 列出可用层 + 当前模型 spec 的生效层。应答 `EventMsg::PromptList`。
    List,
    /// 重扫提示词库(bootstrap 时记录的目录序),让用户对
    /// `.zhuye/prompts/*.md` 的修改即时生效。应答 `EventMsg::PromptSwitched`。
    /// `layer` 保留槽位:未来支持强制聚焦某层;`None` = 全量重扫。
    Switch { layer: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PromptAction 两个变体的 serde 往返(协议 §7:新增变体必须带测试)。
    #[test]
    fn serde_roundtrip_prompt_action() {
        for (action, tag) in [
            (PromptAction::List, "list"),
            (
                PromptAction::Switch {
                    layer: Some("claude".into()),
                },
                "switch",
            ),
            (PromptAction::Switch { layer: None }, "switch"),
        ] {
            let json = serde_json::to_string(&action).unwrap();
            assert!(json.contains(tag), "got: {json}");
            let back: PromptAction = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }
}
