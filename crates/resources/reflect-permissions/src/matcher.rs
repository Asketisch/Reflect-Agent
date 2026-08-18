//! `matcher` —— glob / shell 规则匹配(P2 `permission-rules`)。
//!
//! 在精确 `tool` 匹配之上扩展:
//! - `tool_glob`:工具名 glob(如 `Web*`)
//! - `shell_pattern`:Bash 命令 glob(仅 `Bash`/`bash` 工具生效)

use globset::Glob;

use crate::rules::{PermissionAction, PermissionRule, RuleMatch};

/// 带上下文的规则评估 —— 支持 tool glob 与 Bash shell 模式。
///
/// 匹配顺序与 v1.x 一致:按 rules 数组顺序,**第一条**命中即返回。
/// shell 规则仅在 `bash_command` 有值且工具名与规则 `tool` 一致(忽略大小写)时参与。
pub fn evaluate_with_context(
    rules: &[PermissionRule],
    tool_name: &str,
    bash_command: Option<&str>,
) -> RuleMatch {
    let mut matched = RuleMatch::NoMatch;
    for rule in rules {
        if let Some(m) = match_rule(rule, tool_name, bash_command) {
            if m == RuleMatch::Deny {
                return RuleMatch::Deny;
            }
            if matched == RuleMatch::NoMatch {
                matched = m;
            }
        }
    }
    matched
}

fn match_rule(
    rule: &PermissionRule,
    tool_name: &str,
    bash_command: Option<&str>,
) -> Option<RuleMatch> {
    // shell 规则优先:需要命令上下文。
    if let Some(pat) = rule.shell_pattern.as_deref() {
        let cmd = bash_command?;
        if !tool_names_equal(&rule.tool, tool_name) {
            return None;
        }
        let glob = Glob::new(pat).ok()?;
        if glob.compile_matcher().is_match(cmd) {
            return Some(action_to_match(rule.action));
        }
        return None;
    }

    // tool_glob 规则。
    if let Some(glob_str) = rule.tool_glob.as_deref() {
        let glob = Glob::new(glob_str).ok()?;
        if glob.compile_matcher().is_match(tool_name) {
            return Some(action_to_match(rule.action));
        }
        return None;
    }

    // 精确 tool 匹配(v1.x 行为)。
    // 大小写不敏感:与 shell_pattern 分支(tool_names_equal)及模块文档
    // 声明("忽略大小写")保持一致,让 `allow = ["Bash"]` 能匹配工具上报的
    // `"bash"`(反之亦然)。原 `==` 比较是大小写敏感,导致纯工具名放行规则
    // 对实际工具名(`"bash"`)静默失配。
    if tool_names_equal(&rule.tool, tool_name) {
        return Some(action_to_match(rule.action));
    }
    None
}

fn tool_names_equal(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

fn action_to_match(action: PermissionAction) -> RuleMatch {
    match action {
        PermissionAction::Allow => RuleMatch::Allow,
        PermissionAction::Deny => RuleMatch::Deny,
        PermissionAction::Ask => RuleMatch::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::PermissionRule;

    fn rule(tool: &str, action: PermissionAction) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            action,
            tool_glob: None,
            shell_pattern: None,
        }
    }

    #[test]
    fn tool_glob_matches_prefix() {
        let rules = vec![PermissionRule {
            tool: String::new(),
            action: PermissionAction::Allow,
            tool_glob: Some("Web*".into()),
            shell_pattern: None,
        }];
        assert_eq!(
            evaluate_with_context(&rules, "WebFetch", None),
            RuleMatch::Allow
        );
        assert_eq!(
            evaluate_with_context(&rules, "Read", None),
            RuleMatch::NoMatch
        );
    }

    #[test]
    fn shell_pattern_matches_bash_command() {
        let rules = vec![PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
        }];
        assert_eq!(
            evaluate_with_context(&rules, "Bash", Some("git status")),
            RuleMatch::Allow
        );
        assert_eq!(
            evaluate_with_context(&rules, "Bash", Some("rm -rf /")),
            RuleMatch::NoMatch
        );
        // 非 Bash 工具不触发 shell 规则。
        assert_eq!(
            evaluate_with_context(&rules, "Write", Some("git status")),
            RuleMatch::NoMatch
        );
    }

    #[test]
    fn exact_tool_still_works() {
        let rules = vec![rule("Read", PermissionAction::Deny)];
        assert_eq!(evaluate_with_context(&rules, "Read", None), RuleMatch::Deny);
    }

    /// 回归:精确 tool 匹配必须大小写不敏感 —— `allow = ["Bash"]` 规则
    /// 应能放行工具实际上报的 `"bash"`(反之亦然)。原实现用 `==` 比较,
    /// 对 `"Bash" vs "bash"` 静默失配,导致权限规则失效、审批框误弹。
    #[test]
    fn allow_rule_matches_case_insensitive() {
        let rules = vec![rule("Bash", PermissionAction::Allow)];
        // 规则写 "Bash",工具名各种大小写都应命中 Allow。
        assert_eq!(
            evaluate_with_context(&rules, "bash", None),
            RuleMatch::Allow
        );
        assert_eq!(
            evaluate_with_context(&rules, "BASH", None),
            RuleMatch::Allow
        );
        assert_eq!(
            evaluate_with_context(&rules, "Bash", None),
            RuleMatch::Allow
        );
        // 反过来:规则写小写,工具名大写也匹配。
        let rules_lower = vec![rule("read", PermissionAction::Deny)];
        assert_eq!(
            evaluate_with_context(&rules_lower, "Read", None),
            RuleMatch::Deny
        );
        // 不相关工具名仍不匹配。
        assert_eq!(
            evaluate_with_context(&rules, "read", None),
            RuleMatch::NoMatch
        );
    }

    #[test]
    fn explicit_deny_wins_across_rule_kinds() {
        let rules = vec![
            rule("Bash", PermissionAction::Deny),
            PermissionRule {
                tool: "Bash".into(),
                action: PermissionAction::Allow,
                tool_glob: None,
                shell_pattern: Some("git *".into()),
            },
        ];
        assert_eq!(
            evaluate_with_context(&rules, "Bash", Some("git pull")),
            RuleMatch::Deny,
            "显式 deny 必须优先"
        );
    }
}
