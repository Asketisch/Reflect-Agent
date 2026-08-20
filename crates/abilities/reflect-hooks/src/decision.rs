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

/// 把(可能 `Combined` 的)合并决策按优先级展开后的有效结果。
///
/// `HookEngine::dispatch` 返回的是 `merge` 之后的决策:单个 hook 返回
/// `Combined([InjectMessage, Deny])` 时,合并产物仍是含 `Deny` 叶子的
/// `Combined`。调用方若直接 `match` 顶层变体,`Combined` 会落进通配
/// 分支 —— `Deny` 被静默丢弃(Stop hook 否决失效 / PreToolUse 否决的工具
/// 照常执行)。本结构让调用方只关心「有效结果」,而无需关心决策的嵌套形态。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ResolvedDecision {
    /// 若任一叶子为 `Deny` 则 `Some`(取第一个原因)。
    pub deny_reason: Option<String>,
    /// 若无 `Deny` 且任一叶子为 `Ask` 则 `Some`。
    pub ask_reason: Option<String>,
    /// 参数替换;多个 `ModifyArgs` 时后者覆盖前者(与 `merge` 语义一致)。
    pub modified_args: Option<serde_json::Value>,
    /// 所有 `InjectMessage` 叶子的内容(按出现序)。
    pub injected: Vec<SystemMessage>,
    /// 若任一叶子为 `PermissionOverride` 则 `Some`。
    pub permission_override: Option<PermissionMode>,
}

impl ResolvedDecision {
    /// 是否有效否决(`Deny` 优先于一切)。
    pub fn denied(&self) -> bool {
        self.deny_reason.is_some()
    }
}

impl HookDecision {
    /// 递归展平 `Combined` 并按优先级归类所有叶子:
    /// `Deny > Ask > ModifyArgs(后者胜) > InjectMessage(全部收集)`。
    ///
    /// 这是 `dispatch` 结果的规范消费入口 —— 无论决策是单变体还是
    /// `Combined`,调用方都应经此取有效结果,避免顶层 `match` 把
    /// `Combined` 当「无操作」丢弃。
    pub fn resolve(&self) -> ResolvedDecision {
        let mut out = ResolvedDecision::default();
        Self::resolve_into(self, &mut out);
        out
    }

    /// 按叶子**出现顺序** DFS 归类(栈式遍历会反转顺序,导致
    /// 「第一个 Deny 生效 / 最后一个 ModifyArgs 生效」的语义错乱)。
    fn resolve_into(d: &HookDecision, out: &mut ResolvedDecision) {
        match d {
            HookDecision::Combined(inner) => {
                for leaf in inner {
                    Self::resolve_into(leaf, out);
                }
            }
            HookDecision::Deny { reason } => {
                if out.deny_reason.is_none() {
                    out.deny_reason = Some(reason.clone());
                }
            }
            HookDecision::Ask { reason } => {
                if out.ask_reason.is_none() {
                    out.ask_reason = Some(reason.clone());
                }
            }
            HookDecision::ModifyArgs(v) => {
                out.modified_args = Some(v.clone());
            }
            HookDecision::InjectMessage(m) => {
                out.injected.push(m.clone());
            }
            HookDecision::PermissionOverride(m) => {
                out.permission_override = Some(*m);
            }
            HookDecision::Allow => {}
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

    #[test]
    fn resolve_bare_allow_is_empty() {
        let r = HookDecision::Allow.resolve();
        assert!(!r.denied());
        assert_eq!(r, ResolvedDecision::default());
    }

    #[test]
    fn resolve_bare_deny_carries_reason() {
        let r = HookDecision::Deny {
            reason: "nope".into(),
        }
        .resolve();
        assert!(r.denied());
        assert_eq!(r.deny_reason.as_deref(), Some("nope"));
        assert!(r.ask_reason.is_none());
    }

    #[test]
    fn resolve_combined_inject_and_deny() {
        // 内置 Stop hook 的典型形态:`Combined([InjectMessage, Deny])`。
        // 修复前调用方顶层 `match` 把这种 `Combined` 当「无操作」,
        // `Deny` 被静默丢弃 —— 本用例锁定 `resolve()` 能正确提取。
        let d = HookDecision::Combined(vec![
            HookDecision::InjectMessage(SystemMessage::new("tests failing: 2 failed")),
            HookDecision::Deny {
                reason: "tests failing".into(),
            },
        ]);
        let r = d.resolve();
        assert!(r.denied());
        assert_eq!(r.deny_reason.as_deref(), Some("tests failing"));
        assert_eq!(r.injected.len(), 1);
        assert_eq!(r.injected[0].content, "tests failing: 2 failed");
    }

    #[test]
    fn resolve_nested_combined_recurses() {
        // `merge` 目前不产生嵌套,但 `resolve` 契约上应能处理任意深度。
        let d = HookDecision::Combined(vec![
            HookDecision::Combined(vec![HookDecision::Deny {
                reason: "deep".into(),
            }]),
            HookDecision::InjectMessage(SystemMessage::new("note")),
        ]);
        let r = d.resolve();
        assert!(r.denied());
        assert_eq!(r.deny_reason.as_deref(), Some("deep"));
        assert_eq!(r.injected.len(), 1);
    }

    #[test]
    fn resolve_deny_takes_precedence_over_ask() {
        let d = HookDecision::Combined(vec![
            HookDecision::Ask {
                reason: "risky".into(),
            },
            HookDecision::Deny {
                reason: "blocked".into(),
            },
        ]);
        let r = d.resolve();
        assert!(r.denied());
        assert_eq!(r.deny_reason.as_deref(), Some("blocked"));
        // Ask 信息仍保留(由队列决定在 Deny 之后是否忽略)。
        assert_eq!(r.ask_reason.as_deref(), Some("risky"));
    }

    #[test]
    fn resolve_modify_args_last_wins() {
        let d = HookDecision::Combined(vec![
            HookDecision::ModifyArgs(serde_json::json!({"a": 1})),
            HookDecision::ModifyArgs(serde_json::json!({"a": 2})),
        ]);
        let r = d.resolve();
        assert_eq!(r.modified_args, Some(serde_json::json!({"a": 2})));
    }
}
