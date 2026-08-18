//! `rules` —— 纯数据:permission rule + 评估函数。
//!
//! 与 IO 解耦,可在 `cfg(test)` 之外任意环境使用(例如 CLI 子命令
//! `reflect permissions ls` 直接 evaluate 而不必读 store)。

use serde::{Deserialize, Serialize};

/// 工具权限动作:v1.x 三态 enum,直接对应 ApprovalGate 的 short-circuit
/// 路径(Allow / Deny / Ask)。
///
/// 序列化用 snake_case 字符串,匹配 `~/.reflect/permissions.toml` 的手
/// 编辑体验(用户写 `action = "allow"`,不是 `action = "Allow"`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    /// 显式允许:gate 短路,跳过 modal。
    Allow,
    /// 显式拒绝:gate 短路,直接 deny。
    Deny,
    /// 强制弹 modal(覆盖默认 `PermissionMode` 的 `AcceptEdits` 等)。
    Ask,
}

/// 一条规则 —— "对 `tool` 这一类工具应用 `action`"。
///
/// v1.x 字段:`tool` + `action`。P2 扩展:
/// - `tool_glob`:工具名 glob(与 `tool` 二选一,优先 glob)
/// - `shell_pattern`:Bash 命令 glob(需配合 `tool = "Bash"`)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 工具名(精确匹配)。例:`"Bash"` / `"Write"` / `"Read"`。
    #[serde(default)]
    pub tool: String,
    pub action: PermissionAction,
    /// 工具名 glob(如 `Web*`)。设置后忽略精确 `tool` 字段的匹配语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_glob: Option<String>,
    /// Bash 命令 glob(如 `git *`)。仅对 Bash 工具 + 有命令上下文时生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_pattern: Option<String>,
}

/// Resolver 评估结果:`Allow` 短路 / `Deny` 短路 / `Ask` 强制 prompt /
/// `NoMatch` 没规则,走默认审批路径(由 ApprovalGate 决定 fallback)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMatch {
    Allow,
    Deny,
    Ask,
    /// 未匹配任何规则 —— 上层按 `PermissionMode` 默认行为处理。
    NoMatch,
}

/// 在 `rules` 中找 `tool_name` 对应的第一条规则,返回其 action 映射。
/// 找不到 → `NoMatch`(不是 `Ask` —— "未配置" 与 "强制 prompt" 是不同
/// 语义,resolver 应区分)。
///
/// 选**第一条**而不是"最具体"是因为 v1.x 无 glob / scope 概念,多条同
/// tool 规则中,用户 add 的顺序隐式定 precedence(`add` 总是 append)。
pub fn evaluate(rules: &[PermissionRule], tool_name: &str) -> RuleMatch {
    crate::matcher::evaluate_with_context(rules, tool_name, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(tool: &str, a: PermissionAction) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            action: a,
            tool_glob: None,
            shell_pattern: None,
        }
    }

    #[test]
    fn evaluate_empty_rules_returns_no_match() {
        assert_eq!(evaluate(&[], "Bash"), RuleMatch::NoMatch);
    }

    #[test]
    fn evaluate_matches_first_rule() {
        let rules = vec![r("Bash", PermissionAction::Allow)];
        assert_eq!(evaluate(&rules, "Bash"), RuleMatch::Allow);
    }

    #[test]
    fn evaluate_maps_actions_correctly() {
        assert_eq!(
            evaluate(&[r("X", PermissionAction::Allow)], "X"),
            RuleMatch::Allow
        );
        assert_eq!(
            evaluate(&[r("X", PermissionAction::Deny)], "X"),
            RuleMatch::Deny
        );
        assert_eq!(
            evaluate(&[r("X", PermissionAction::Ask)], "X"),
            RuleMatch::Ask
        );
    }

    #[test]
    fn evaluate_explicit_deny_wins() {
        let rules = vec![
            r("Bash", PermissionAction::Allow),
            r("Bash", PermissionAction::Deny),
        ];
        assert_eq!(evaluate(&rules, "Bash"), RuleMatch::Deny);
    }

    #[test]
    fn evaluate_does_not_substring_match() {
        // 防止 "Bash" 匹配 "BashExec" 这种 partial 行为。
        let rules = vec![r("Bash", PermissionAction::Allow)];
        assert_eq!(evaluate(&rules, "BashExec"), RuleMatch::NoMatch);
    }
}
