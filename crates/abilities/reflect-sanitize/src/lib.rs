//! # reflect-sanitize
//!
//! 轻量级密钥脱敏叶 crate。只依赖 `regex` 与 `once_cell`,
//! 供 `reflect-rollout` / `reflect-telemetry` 这类**禁止依赖完整
//! `reflect-tools` 工具栈**的落盘层复用同一套密钥 pattern。
//!
//! ## 与 `reflect_tools::sanitize` 的关系
//!
//! `reflect_tools::sanitize` 是「工具输出进 LLM 上下文前的脱敏节点」,
//! 额外持有 `Sanitizer` 配置(自定义 marker、`extra_patterns`、
//! `ContentBlock` / `ToolOutput` 递归入口),依赖 `reflect-protocol`。
//!
//! 本 crate 只复制同一份 10 类默认 pattern + `redact_string` 入口,
//! 避免在 rollout / telemetry 的依赖图里拉入工具运行时。pattern 字面量
//! 与 `reflect_tools::sanitize::default_patterns` 完全一致(同样的顺序、
//! 同样的 marker `[REDACTED]` / `[REDACTED:<type>]`),保证两处行为对齐。
//!
//! ## 默认 pattern(详见 [`default_patterns`])
//!
//! 1. `KEY_ASSIGN` —— `KEY=value` / `password: xxx` 类赋值
//! 2. `BEARER_TOKEN` —— HTTP `Authorization: Bearer xxx`
//! 3. `AWS_ACCESS_KEY` —— `AKIA*` / `ASIA*` 16-char 标识
//! 4. `OPENAI_KEY` —— `sk-...` 20+ 字符
//! 5. `GITHUB_TOKEN` —— `ghp_*` / `gho_*` / `ghs_*` / `ghr_*` / `ghu_*`
//! 6. `ANTHROPIC_KEY` —— `sk-ant-...` 20+ 字符
//! 7. `PRIVATE_KEY_BLOCK` —— `-----BEGIN ... PRIVATE KEY-----` 整块
//! 8. `JWT` —— `eyJ*.eyJ*.*` 三段 base64url
//! 9. `DB_URL` —— `postgres://...` / `mongodb://...` 等连接串
//! 10. `SLACK_TOKEN` —— `xox[abprs]-...`
//!
//! ## 幂等性
//!
//! 默认 pattern 的值字符类显式排除 `[` 与 `]`,因此 `[REDACTED]`
//! marker 不会触发再次匹配,二次调用结果与一次一致。落盘层先做密钥脱敏、
//! 再做长度截断是安全的(截断 marker `[redacted]` 同样不含被匹配的密钥
//! 字面量)。

use once_cell::sync::Lazy;
use regex::{Captures, Regex};

/// 默认脱敏 marker。与 `reflect_tools::sanitize::DEFAULT_MARKER` 对齐。
pub const DEFAULT_MARKER: &str = "[REDACTED]";

/// 单个编译后的 pattern + 替换策略。
#[derive(Debug)]
pub struct CompiledPattern {
    /// 调试 / 日志用稳定 id(`"KEY_ASSIGN"` / `"AWS_ACCESS_KEY"` 等)。
    pub id: &'static str,
    regex: Regex,
    /// `(&Captures, marker) -> String` —— 用 capture group 拼装替换文本。
    replace: fn(&Captures, marker: &str) -> String,
}

/// 把 `[REDACTED]` 类带括号 marker 转成 `[REDACTED:<type>]` 形式。
fn typed_marker(marker: &str, type_id: &str) -> String {
    let inner = marker.strip_prefix('[').unwrap_or(marker);
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    format!("[{inner}:{type_id}]")
}

fn replace_bearer(_caps: &Captures, marker: &str) -> String {
    format!("Bearer {marker}")
}
fn replace_aws(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "aws_key")
}
fn replace_openai(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "openai_key")
}
fn replace_github(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "github_token")
}
fn replace_anthropic(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "anthropic_key")
}
fn replace_private_key(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "private_key")
}
fn replace_jwt(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "jwt")
}
fn replace_db_url(caps: &Captures, marker: &str) -> String {
    let scheme = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    format!("{scheme}{marker}")
}
fn replace_slack(_caps: &Captures, marker: &str) -> String {
    typed_marker(marker, "slack_token")
}

/// 构造全部 10 个默认 pattern。每个 regex 都经过手工审查、无回溯爆炸。
///
/// 顺序关键:具体 provider pattern 必须放在 `KEY_ASSIGN` 之前,否则
/// `KEY=value` 类赋值会把它们整段吞掉;`ANTHROPIC_KEY` 必须先于
/// `OPENAI_KEY`(`sk-ant-...` 是 `sk-...` 子集)。
pub fn default_patterns() -> Vec<CompiledPattern> {
    vec![
        CompiledPattern {
            id: "AWS_ACCESS_KEY",
            regex: Regex::new(r"\b(AKIA|ASIA)[A-Z0-9]{16}\b")
                .expect("AWS_ACCESS_KEY regex must compile"),
            replace: replace_aws,
        },
        CompiledPattern {
            id: "ANTHROPIC_KEY",
            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b")
                .expect("ANTHROPIC_KEY regex must compile"),
            replace: replace_anthropic,
        },
        CompiledPattern {
            id: "OPENAI_KEY",
            regex: Regex::new(r"\bsk-[A-Za-z0-9_\-]{20,}\b")
                .expect("OPENAI_KEY regex must compile"),
            replace: replace_openai,
        },
        CompiledPattern {
            id: "GITHUB_TOKEN",
            regex: Regex::new(r"\b(ghp|gho|ghs|ghr|ghu)_[A-Za-z0-9]{30,}\b")
                .expect("GITHUB_TOKEN regex must compile"),
            replace: replace_github,
        },
        CompiledPattern {
            id: "PRIVATE_KEY_BLOCK",
            regex: Regex::new(
                r"-----BEGIN (RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----[\s\S]+?-----END (RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----",
            )
            .expect("PRIVATE_KEY_BLOCK regex must compile"),
            replace: replace_private_key,
        },
        CompiledPattern {
            id: "JWT",
            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+")
                .expect("JWT regex must compile"),
            replace: replace_jwt,
        },
        CompiledPattern {
            id: "DB_URL",
            regex: Regex::new(
                r#"\b((?:postgres|postgresql|mysql|mongodb(\+srv)?|redis|amqp|amqps)://)[^\s"'<>]+"#,
            )
            .expect("DB_URL regex must compile"),
            replace: replace_db_url,
        },
        CompiledPattern {
            id: "SLACK_TOKEN",
            regex: Regex::new(r"\bxox[abprs]-[A-Za-z0-9\-]{10,}\b")
                .expect("SLACK_TOKEN regex must compile"),
            replace: replace_slack,
        },
        CompiledPattern {
            id: "BEARER_TOKEN",
            regex: Regex::new(r"(?i)\bBearer\s+([A-Za-z0-9\-._~+/]+=*)")
                .expect("BEARER_TOKEN regex must compile"),
            replace: replace_bearer,
        },
        CompiledPattern {
            id: "KEY_ASSIGN",
            regex: Regex::new(
                r#"(?i)(^|[\s_\-])(key|token|secret|password|credential|passwd|pwd)\s*([=:])\s*["']?([^ \t\r\n"',;)\[\]]+)"#,
            )
            .expect("KEY_ASSIGN regex must compile"),
            replace: |caps, marker| {
                let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let key = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                let sep = caps.get(3).map(|m| m.as_str()).unwrap_or("=");
                format!("{prefix}{key}{sep}{marker}")
            },
        },
    ]
}

/// 进程级缓存的默认 pattern 集合 + marker。`OnceLock` 保证只编译一次。
static DEFAULT_SANITIZER: Lazy<(Vec<CompiledPattern>, &'static str)> =
    Lazy::new(|| (default_patterns(), DEFAULT_MARKER));

/// short-circuit 阈值(与 `reflect_tools::sanitize` 对齐)。
const SHORT_CIRCUIT_MIN_LEN: usize = 32;

/// 文本明显不含任何密钥字面量 —— 直接返回,跳过正则遍历。
fn short_circuit_ok(text: &str) -> bool {
    text.len() < SHORT_CIRCUIT_MIN_LEN
        && !text.contains('=')
        && !text.contains(':')
        && !text.contains("Bearer")
        && !text.contains("BEGIN")
        && !text.contains("sk-")
        && !text.contains("ghp_")
        && !text.contains("AKIA")
        && !text.contains("ASIA")
        && !text.contains("eyJ")
        && !text.contains("xox")
        && !text.contains("://")
}

/// 对单段文本用默认 pattern 集合脱敏,marker 为 [`DEFAULT_MARKER`]。
///
/// 这是 rollout / telemetry 落盘前的唯一脱敏入口。不可变借用、无配置、
/// 零运行时编译开销(`Lazy` 只编译一次)。
pub fn redact_string(text: &str) -> String {
    if short_circuit_ok(text) {
        return text.to_string();
    }
    let (patterns, marker) = &*DEFAULT_SANITIZER;
    let mut out = text.to_string();
    for pat in patterns {
        out = pat
            .regex
            .replace_all(&out, |caps: &Captures| (pat.replace)(caps, marker))
            .into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_key_redacted() {
        assert_eq!(
            redact_string("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"),
            "AWS_ACCESS_KEY_ID=[REDACTED:aws_key]"
        );
    }

    #[test]
    fn bearer_keeps_prefix() {
        assert_eq!(
            redact_string("Authorization: Bearer eyJabc.def.ghi"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn openai_and_anthropic_order() {
        let out = redact_string("sk-ant-api03-abcdefghijklmnopqrstuvwxyz");
        assert!(out.contains("[REDACTED:anthropic_key]"));
        let out = redact_string("sk-abcdefghijklmnopqrstuv");
        assert!(out.contains("[REDACTED:openai_key]"));
    }

    #[test]
    fn github_token_redacted() {
        let out = redact_string("ghp_abcdefghijklmnopqrstuvwxyz0123456789");
        assert!(out.contains("[REDACTED:github_token]"));
    }

    #[test]
    fn db_url_keeps_scheme() {
        assert_eq!(
            redact_string("postgres://user:pass@localhost/db"),
            "postgres://[REDACTED]"
        );
    }

    #[test]
    fn short_circuit_noop() {
        assert_eq!(redact_string("ok"), "ok");
        assert_eq!(redact_string("done"), "done");
    }

    #[test]
    fn idempotent() {
        let fixtures = [
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            "Authorization: Bearer abc.def.ghi",
            "sk-abcdefghijklmnopqrstuv",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "postgres://user:pass@host/db",
            "-----BEGIN PRIVATE KEY-----\nABC\n-----END PRIVATE KEY-----",
            "xoxb-1234567890-abcdef",
        ];
        for raw in fixtures {
            let once = redact_string(raw);
            let twice = redact_string(&once);
            assert_eq!(
                once, twice,
                "not idempotent: {raw:?} -> {once:?} -> {twice:?}"
            );
        }
    }

    #[test]
    fn default_patterns_count_is_ten() {
        assert_eq!(default_patterns().len(), 10);
    }
}
