//! 工具输出密钥脱敏 —— 工具输出进入 LLM 上下文前的唯一脱敏节点。
//!
//! ## 契约
//!
//! 本模块在 [`crate::queue::ToolExecutionQueue::execute_single`] 的
//! `Ok(Ok(output))` 分支、`tool.execute(...)` 返回之后、`PostToolUse`
//! 钩子派发之前被调用,把 [`reflect_protocol::ToolOutput`] 内每个
//! [`ContentBlock`] 的可文本字段按既定 pattern 替换为脱敏 marker。
//!
//! 设计要点:
//!
//! - **位置唯一**:PostToolUse 钩子的 [`HookDecision`](reflect_hooks::HookDecision)
//!   无法 mutate `result`(详见 `reflect-hooks/src/decision.rs`),
//!   所以脱敏必须做在 queue 层,不能委托给 hook。
//! - **PostToolUse 钩子只看到脱敏后版本**:LangfuseTracker 等 telemetry
//!   钩子不会泄露原始密钥到 tracing span / rollout 落盘。
//! - **错误路径不二次扫描**:`Ok(Err(_))` 与 timeout 分支合成的
//!   `ContentBlock::text("...")` 是工具错误模板,不携带真实密钥,
//!   重复扫描会引入 false positive 与 CPU 浪费。
//! - **正则无回溯爆炸风险**:所有 pattern 的量词均有字面量上界(如
//!   `{16}` / `{20,}`);`PRIVATE_KEY_BLOCK` 用 non-greedy `+?` 以
//!   `-----END ... PRIVATE KEY-----` 字面量收尾,无灾难性回溯。
//!
//! ## 默认 pattern
//!
//! 详见 [`default_patterns`],目前覆盖 10 类常见密钥格式:
//!
//! 1. `KEY_ASSIGN` —— `KEY=value` / `password: xxx` 类赋值
//! 2. `BEARER_TOKEN` —— HTTP 头部 `Authorization: Bearer xxx`
//! 3. `AWS_ACCESS_KEY` —— `AKIA*` / `ASIA*` 16-char 标识
//! 4. `OPENAI_KEY` —— `sk-...` 20+ 字符
//! 5. `GITHUB_TOKEN` —— `ghp_*` / `gho_*` / `ghs_*` / `ghr_*` / `ghu_*`
//! 6. `ANTHROPIC_KEY` —— `sk-ant-...` 20+ 字符
//! 7. `PRIVATE_KEY_BLOCK` —— `-----BEGIN ... PRIVATE KEY-----` 整块
//! 8. `JWT` —— `eyJ*.eyJ*.*` 三段 base64url
//! 9. `DB_URL` —— `postgres://...` / `mongodb://...` 等连接串
//! 10. `SLACK_TOKEN` —— Slack token,`xox[abprs]-...` 形式
//!
//! ## 幂等性
//!
//! 默认 pattern 的值字符类显式排除 `[` 与 `]`,因此 `[REDACTED]`
//! marker 不会触发再次匹配,二次调用结果与一次一致。
//! 用户通过 `extra_patterns` 提供的自定义 pattern 不保证此性质,
//! 文档提醒用户自行测试。

use std::sync::Arc;

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use reflect_protocol::{ContentBlock, ToolOutput};

/// 默认脱敏 marker。出现在脱敏后的文本中,模型应识别为「此处曾有密钥」。
///
/// 自定义 marker 时务必保证 marker 文本不会再次命中任何 pattern ——
/// 例如 marker 含 `[` / `]` 或其他在 pattern 值字符类中被排除的字符。
pub const DEFAULT_MARKER: &str = "[REDACTED]";

/// 工具输出脱敏器。内部状态用 [`Arc`] 共享,克隆廉价。
///
/// 通过 [`Sanitizer::with_defaults`] / [`Sanitizer::from_config`] /
/// [`Sanitizer::disabled`] 三种构造方式。
#[derive(Debug, Clone)]
pub struct Sanitizer {
    inner: Arc<SanitizerInner>,
}

#[derive(Debug)]
struct SanitizerInner {
    /// 编译后的 pattern 列表。空向量表示「no-op」(`disabled()` 情形)。
    patterns: Vec<CompiledPattern>,
    marker: String,
    /// 用户是否提供 `extra_patterns`。short-circuit 启发式只看默认 pattern
    /// 的 trigger 字面量;若有 extras,保守起见不走 short-circuit。
    has_extras: bool,
}

impl Sanitizer {
    /// 用全部 10 个默认 pattern + 默认 marker 构造。`enabled = true`。
    pub fn with_defaults() -> Self {
        Self {
            inner: Arc::new(SanitizerInner {
                patterns: default_patterns(),
                marker: DEFAULT_MARKER.to_string(),
                has_extras: false,
            }),
        }
    }

    /// 完全关闭脱敏,任何 [`sanitize_text`] 调用直接返回输入副本。
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(SanitizerInner {
                patterns: Vec::new(),
                marker: DEFAULT_MARKER.to_string(),
                has_extras: false,
            }),
        }
    }

    /// 从 [`SanitizeConfig`] 构造。用户配置无效时返回错误。
    pub fn from_config(cfg: &SanitizeConfig) -> Result<Self, SanitizeError> {
        // `enabled = false` 走短路。
        if cfg.enabled == Some(false) {
            return Ok(Self::disabled());
        }

        let marker = cfg
            .marker
            .clone()
            .unwrap_or_else(|| DEFAULT_MARKER.to_string());

        // 按 `disable_default_patterns` 与 `extra_patterns` 决定 pattern 列表。
        let mut patterns: Vec<CompiledPattern> = if cfg.disable_default_patterns != Some(true) {
            default_patterns()
        } else {
            Vec::new()
        };

        if let Some(extras) = &cfg.extra_patterns {
            for (i, raw) in extras.iter().enumerate() {
                let regex = Regex::new(raw).map_err(|e| SanitizeError::InvalidPattern {
                    index: i,
                    pattern_source: raw.clone(),
                    message: e.to_string(),
                })?;
                patterns.push(CompiledPattern {
                    id: "<extra>",
                    regex,
                    // 默认额外 pattern 把整段匹配直接替换为 marker。
                    replace: replace_whole,
                });
            }
        }

        Ok(Self {
            inner: Arc::new(SanitizerInner {
                patterns,
                marker,
                has_extras: cfg.extra_patterns.as_ref().is_some_and(|v| !v.is_empty()),
            }),
        })
    }

    /// 当前是否启用(有 pattern 可跑)。`disabled()` 返回 `false`。
    pub fn is_enabled(&self) -> bool {
        !self.inner.patterns.is_empty()
    }

    /// 当前 marker 文本。
    pub fn marker(&self) -> &str {
        &self.inner.marker
    }

    /// 已注册的 pattern 数量(供 `/hooks ls` 与 `docs/sanitize.md` 展示)。
    pub fn pattern_count(&self) -> usize {
        self.inner.patterns.len()
    }
}

/// 配置镜像,对应 TOML `[sanitize]` 段。所有字段 `Option` —— 缺省即默认。
///
/// 详见 `docs/sanitize.md` 与 `reflect-config/src/schema.rs::SanitizeSection`。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SanitizeConfig {
    /// `false` 显式关闭脱敏(等同 [`Sanitizer::disabled`])。
    pub enabled: Option<bool>,
    /// 覆盖默认 `[REDACTED]` marker。
    pub marker: Option<String>,
    /// `true` 时不加载 10 个默认 pattern,只跑 `extra_patterns`。
    pub disable_default_patterns: Option<bool>,
    /// 用户补充的额外 pattern。整段匹配被替换为 marker,无 capture group 语义。
    pub extra_patterns: Option<Vec<String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SanitizeError {
    /// 用户 `extra_patterns[i]` 不是合法 regex。
    #[error("invalid extra_patterns[{index}]: {pattern_source:?} ({message})")]
    InvalidPattern {
        index: usize,
        /// 触发错误的原始正则字符串(命名避开 `source`,免得被 thiserror
        /// 当作 `#[source]` 处理)。
        pattern_source: String,
        /// `regex::Error` 的人类可读消息。
        message: String,
    },
}

/// 单个编译后的 pattern + 替换策略。
#[derive(Debug)]
pub struct CompiledPattern {
    /// 调试 / 日志用稳定 id(`"KEY_ASSIGN"` / `"AWS_ACCESS_KEY"` 等)。
    #[allow(dead_code)]
    id: &'static str,
    regex: Regex,
    /// `(&Captures, marker) -> String` —— 用 capture group 拼装替换文本。
    /// marker 作为第二参数传入,避免每个 pattern 持有 marker 副本。
    replace: fn(&Captures, marker: &str) -> String,
}

// ── 默认 pattern 集合 ────────────────────────────────────────────
//
// 注意:值字符类显式排除 `[` 与 `]`,确保默认 marker `[REDACTED]` 不
// 会触发再次匹配,达到二次调用幂等。

/// 整段匹配直接替换为 marker。给用户 `extra_patterns` 用。
fn replace_whole(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    marker.to_string()
}

/// HTTP `Authorization: Bearer xxx`。保留 `Bearer ` 前缀,替换 token。
fn replace_bearer(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    format!("Bearer {marker}")
}

/// 把 `[REDACTED]` 类带括号 marker 转成 `[REDACTED:<type>]` 形式。
///
/// 用户自定义 marker `[HIDDEN]` 会得到 `[HIDDEN:aws_key]`。
/// 无括号的 marker(如 `REDACTED`)则得到 `[REDACTED:aws_key]`。
fn typed_marker(marker: &str, type_id: &str) -> String {
    let inner = marker.strip_prefix('[').unwrap_or(marker);
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    format!("[{inner}:{type_id}]")
}

/// AWS access key。直接整段替换,带类型后缀。
fn replace_aws(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "aws_key")
}

/// OpenAI API key。直接整段替换,带类型后缀。
fn replace_openai(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "openai_key")
}

/// GitHub token。直接整段替换,带类型后缀。
fn replace_github(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "github_token")
}

/// Anthropic API key。直接整段替换,带类型后缀。
fn replace_anthropic(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "anthropic_key")
}

/// PEM 私钥整块。
fn replace_private_key(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "private_key")
}

/// JWT。
fn replace_jwt(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "jwt")
}

/// 数据库连接串。保留 scheme 前缀,替换后续整段(含 user:pass@host)。
fn replace_db_url(caps: &Captures, marker: &str) -> String {
    let scheme = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    format!("{scheme}{marker}")
}

/// Slack token。直接整段替换,带类型后缀。
fn replace_slack(caps: &Captures, marker: &str) -> String {
    let _ = caps;
    typed_marker(marker, "slack_token")
}

/// 构造全部 10 个默认 pattern。每个的 regex 都经过手工审查,确认无回溯爆炸。
///
/// 顺序关键:`KEY_ASSIGN` 必须放在所有具体 provider pattern 之后,
/// 否则 `AWS_ACCESS_KEY_ID=AKIA...` 会被 `KEY_ASSIGN` 整段吞掉,
/// 后续的 `AWS_ACCESS_KEY` 看不到原始 key。下方排列即此顺序。
pub fn default_patterns() -> Vec<CompiledPattern> {
    vec![
        // ── 具体 provider key pattern(必须先于 KEY_ASSIGN) ──────
        CompiledPattern {
            id: "AWS_ACCESS_KEY",
            regex: Regex::new(r"\b(AKIA|ASIA)[A-Z0-9]{16}\b")
                .expect("AWS_ACCESS_KEY regex must compile"),
            replace: replace_aws,
        },
        // ANTHROPIC_KEY 必须先于 OPENAI_KEY —— `sk-ant-...` 是 `sk-...`
        // 的子集,否则 OPENAI 会先抢跑把整段替换成 `[REDACTED:openai_key]`。
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
        // KEY_ASSIGN —— 区分大小写不敏感;要求 `(key|token|...)=value` 形式。
        // 值字符类排除 `[`,`]`,以保证 `[REDACTED]` 不会被二次匹配。
        // 注意:Rust 的 `regex` crate 不支持 look-around,所以用
        // 显式 capturing 前缀字符的方式代替 `(?<![A-Za-z0-9_])`。
        // 允许 `^` / 空白 / `_` / `-` 作为关键词与前一字符的分隔,既能
        // 命中 `API_KEY=...` / `MY_PASSWORD=...` 这类带前缀的字段,
        // 又能排除 `mykey=...` / `dontkey=...` 这类非密钥字段。
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

// ── short-circuit 快速路径 ───────────────────────────────────────

/// short-circuit 阈值 —— 短于这个长度的文本几乎不可能含完整 provider
/// key(prefix + value 加起来最少 30+ 字符),跳过 10 个正则直接返回。
///
/// Review 2026-06-30 P2-3:提为命名常量,加注释解释魔数由来。
///
/// 历史:这个数字从 v1.0.0-rc2 初始实现起就是字面量 `32`,没有任何
/// benchmark 支撑 —— 选 32 是因为它比所有默认 pattern 的"prefix +
/// minimum value length"和稍大(AWS AKIA + 16 = 20,加前后空白 32
/// 足够),又不会太长到把含密钥的工具输出错误跳过。如果未来要把
/// pattern 列表扩充到支持更长 prefix(比如 JWT 的 `eyJ...` 段),需要
/// 重新评估这个数字。
const SHORT_CIRCUIT_MIN_LEN: usize = 32;

/// 文本明显不含任何密钥字面量 —— 直接返回输入,跳过正则编译与遍历。
///
/// 启发式:短文本 + 缺少数值类触发字符,99% 的常见 `ok` / `done` /
/// `from read` 类输出命中此路径。完整模式覆盖仍依赖后续正则。
///
/// 当 sanitizer 含用户 `extra_patterns` 时(`has_extras = true`),
/// 不走 short-circuit —— 用户的自定义 trigger 字面量不可枚举。
fn short_circuit_ok(text: &str, has_extras: bool) -> bool {
    !has_extras
        && text.len() < SHORT_CIRCUIT_MIN_LEN
        // KEY_ASSIGN 同时接受 `=` 与 `:` 作为分隔符,两者都需要触发。
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
        // DB_URL schemes 一律含 `://`,加这个触发即可覆盖
        // postgres / mongodb / mysql / redis / amqp / amqps 等。
        && !text.contains("://")
}

// ── 公共入口 ─────────────────────────────────────────────────────

/// 对单段文本做脱敏。`Sanitizer` 不可变借用,无 clone 开销。
///
/// 若 [`Sanitizer::is_enabled`] 为 `false` 或短路径命中,返回原 `&str`(无 `String` 分配)。
pub fn sanitize_text(text: &str, sanitizer: &Sanitizer) -> String {
    if !sanitizer.is_enabled() || short_circuit_ok(text, sanitizer.inner.has_extras) {
        return text.to_string();
    }
    let mut out = text.to_string();
    for pat in &sanitizer.inner.patterns {
        out = pat
            .regex
            .replace_all(&out, |caps: &Captures| {
                (pat.replace)(caps, &sanitizer.inner.marker)
            })
            .into_owned();
    }
    out
}

/// 对单个 [`ContentBlock`] 做脱敏。`ToolUse` 与 `Image` 不动,`Text` / `Diff` /
/// `ToolResult` 递归处理。
pub fn sanitize_block(block: &mut ContentBlock, sanitizer: &Sanitizer) {
    match block {
        ContentBlock::Text { text } => {
            *text = sanitize_text(text, sanitizer);
        }
        ContentBlock::Diff { unified_diff } => {
            *unified_diff = sanitize_text(unified_diff, sanitizer);
        }
        ContentBlock::ToolResult { output, .. } => {
            // 递归对内层 ToolOutput 脱敏 —— 即使子代理 / 上层包装器
            // 已脱敏过一层,这里再次兜底,避免嵌套结构里的密钥通过
            // `ContentBlock::ToolResult` 边界漏出。
            sanitize_output(output, sanitizer);
        }
        ContentBlock::Image { .. } | ContentBlock::ToolUse { .. } => {
            // 二进制 / 请求类载荷,不动。
        }
    }
}

/// 对 [`ToolOutput`] 整体脱敏 —— 即 `Vec<ContentBlock>` 中每一块。
pub fn sanitize_output(output: &mut ToolOutput, sanitizer: &Sanitizer) {
    for block in output.content.iter_mut() {
        sanitize_block(block, sanitizer);
    }
}

#[cfg(test)]
mod tests;
