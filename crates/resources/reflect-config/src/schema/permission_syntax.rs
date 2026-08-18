//! `permission_syntax` —— Claude Code 式紧凑权限字符串解析糖。
//!
//! 把 `"Bash(git:*)"` / `"Edit"` 这类紧凑字符串解析成底层
//! [`PermissionRule`](reflect_permissions::PermissionRule),
//! 让用户能用类似 Claude Code `settings.json` 的写法配置 shell 命令放行:
//!
//! ```toml
//! [permissions]
//! allow = ["Bash", "Edit", "Write", "Read", "Bash(git:*)", "Bash(npm:*)"]   # 放行列表
//! deny  = ["Bash(curl:*)", "Bash(wget:*)"]                                  # 拒绝列表
//! ```
//!
//! ## 解析规则
//!
//! | 输入 | 解析结果 |
//! |------|----------|
//! | `"Bash"` | 精确 `tool = "Bash"` |
//! | `"Bash(git:*)"` | `tool = "Bash"` + `shell_pattern = "git*"` |
//! | `"Bash(git diff:*)"` | `tool = "Bash"` + `shell_pattern = "git diff*"` |
//! | `"Web*"`(无括号 + 通配) | `tool_glob = "Web*"` |
//! | `"Read(/path)"`(非 Bash 带括号) | 忽略括号内容,仅 `tool = "Read"` |
//!
//! `action` 由调用方按字符串所属数组(allow/deny)传入。底层 matcher / store /
//! gate 完全不用改 —— 这里仅做一层字符串 → 结构体的转换。

use reflect_permissions::{PermissionAction, PermissionRule};

/// Bash 类工具名(忽略大小写)。这类工具的括号内容会生成 `shell_pattern`,
/// 其它工具的括号内容当前不支持参数 glob,仅保留工具名放行。
fn is_bash_like(tool: &str) -> bool {
    tool.eq_ignore_ascii_case("Bash")
}

/// 判断字符串是否含 glob 元字符,用于区分「工具名 glob」与「精确工具名」。
fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains(']')
}

/// 把 Claude 的 `prefix:*` 约定规整成 Reflect 的 `prefix*` glob。
///
/// Claude 用 `:` 作分隔(`git diff:*`),Reflect 的 `globset` 不认 `:`,
/// 这里把尾部 `:*`(或裸 `*`)规整为 `*` 结尾;中间若有 `:` 也转成空格,
/// 适配「命令 + 空格 + 参数」的自然形态。例:
/// - `git:*`   → `git*`    # 转换样例
/// - `git:* ` → `git*`     # 转换样例
/// - `npm:* ` → `npm*`     # 转换样例
/// - `git diff:*` → `git diff*`    # 转换样例
fn globify_claude_shell(inner: &str) -> String {
    // 去掉首尾空白。
    let trimmed = inner.trim();
    // 找到最后一个 `:*`,把其后缀规整为单个 `*`。
    // 同时容忍「无 :」的纯 glob(`git *`)。
    if let Some(idx) = trimmed.rfind(":*") {
        let prefix = trimmed[..idx].trim_end();
        format!("{prefix}*")
    } else {
        // 已是 `prefix*` 形态或精确前缀,直接返回。
        trimmed.to_string()
    }
}

/// 解析单条 Claude 式权限字符串。
///
/// 返回 `None` 表示格式无效(空串 / 括号不闭合 / 工具名为空),调用方可忽略。
/// `action` 决定生成规则的允许/拒绝/询问语义。
pub fn parse_permission_entry(s: &str, action: PermissionAction) -> Option<PermissionRule> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // 拆出 `Tool(args)` 形态。要求左括号在末尾的工具名之后、右括号收尾。
    if let Some(open) = s.find('(') {
        // 必须以 `)` 结尾(容忍尾部空白)。
        let after_open = &s[open + 1..];
        let close = after_open.rfind(')')?;
        // 右括号之后只能有空白,否则视为格式错。
        if !after_open[close + 1..].trim().is_empty() {
            return None;
        }
        let tool = s[..open].trim();
        let inner = after_open[..close].trim();
        if tool.is_empty() {
            return None;
        }
        // 仅 Bash 类工具生成 shell_pattern;其它工具暂不支持参数 glob,
        // 退化为「整工具放行」(Read/Write 路径 glob 属后续扩展)。
        if is_bash_like(tool) && !inner.is_empty() {
            let shell_pattern = globify_claude_shell(inner);
            if shell_pattern.is_empty() {
                return Some(PermissionRule {
                    tool: tool.to_string(),
                    action,
                    tool_glob: None,
                    shell_pattern: None,
                });
            }
            return Some(PermissionRule {
                tool: tool.to_string(),
                action,
                tool_glob: None,
                shell_pattern: Some(shell_pattern),
            });
        }
        // 非 Bash 工具或空括号:忽略括号内容,仅按工具名放行。
        return Some(PermissionRule {
            tool: tool.to_string(),
            action,
            tool_glob: None,
            shell_pattern: None,
        });
    }

    // 无括号:含通配元字符 → tool_glob;否则精确 tool。
    if has_glob_meta(s) {
        Some(PermissionRule {
            tool: String::new(),
            action,
            tool_glob: Some(s.to_string()),
            shell_pattern: None,
        })
    } else {
        Some(PermissionRule {
            tool: s.to_string(),
            action,
            tool_glob: None,
            shell_pattern: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_rule(tool: &str) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: None,
        }
    }

    #[test]
    fn parses_plain_tool_name() {
        assert_eq!(
            parse_permission_entry("Bash", PermissionAction::Allow),
            Some(allow_rule("Bash"))
        );
        assert_eq!(
            parse_permission_entry("Edit", PermissionAction::Allow),
            Some(allow_rule("Edit"))
        );
    }

    #[test]
    fn parses_bash_shell_pattern_with_colon() {
        let r = parse_permission_entry("Bash(git:*)", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.action, PermissionAction::Allow);
        assert_eq!(r.shell_pattern.as_deref(), Some("git*"));
        assert!(r.tool_glob.is_none());
    }

    #[test]
    fn parses_bash_shell_pattern_with_subcommand() {
        // `git diff:*` → `git diff*`    # 转换样例
        let r = parse_permission_entry("Bash(git diff:*)", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.shell_pattern.as_deref(), Some("git diff*"));
    }

    #[test]
    fn parses_bash_plain_glob_no_colon() {
        // 直接写 `git *`(无 :)也兼容。
        let r = parse_permission_entry("Bash(git *)", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.shell_pattern.as_deref(), Some("git *"));
    }

    #[test]
    fn deny_action_propagates() {
        let r = parse_permission_entry("Bash(curl:*)", PermissionAction::Deny).unwrap();
        assert_eq!(r.action, PermissionAction::Deny);
        assert_eq!(r.shell_pattern.as_deref(), Some("curl*"));
    }

    #[test]
    fn non_bash_tool_with_parens_falls_back_to_tool_name() {
        // Read(/path) 当前不支持路径 glob,退化为整工具放行。
        let r = parse_permission_entry("Read(/some/path)", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Read");
        assert!(r.shell_pattern.is_none());
        assert!(r.tool_glob.is_none());
    }

    #[test]
    fn empty_bash_parens_falls_back_to_tool_name() {
        let r = parse_permission_entry("Bash()", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Bash");
        assert!(r.shell_pattern.is_none());
    }

    #[test]
    fn parses_tool_glob_without_parens() {
        let r = parse_permission_entry("Web*", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "");
        assert_eq!(r.tool_glob.as_deref(), Some("Web*"));
        assert!(r.shell_pattern.is_none());
    }

    #[test]
    fn rejects_empty_string() {
        assert_eq!(parse_permission_entry("", PermissionAction::Allow), None);
        assert_eq!(parse_permission_entry("   ", PermissionAction::Allow), None);
    }

    #[test]
    fn rejects_unclosed_paren() {
        // 右括号缺失 → None。
        assert_eq!(
            parse_permission_entry("Bash(git:*", PermissionAction::Allow),
            None
        );
    }

    #[test]
    fn rejects_garbage_after_close() {
        // `) ` 之后有多余字符 → None。
        assert_eq!(
            parse_permission_entry("Bash(git:*)x", PermissionAction::Allow),
            None
        );
    }

    #[test]
    fn trims_whitespace() {
        let r = parse_permission_entry("  Bash  ", PermissionAction::Allow).unwrap();
        assert_eq!(r.tool, "Bash");
    }
}
