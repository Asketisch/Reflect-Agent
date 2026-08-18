//! v1.0.0-rc2 review 2026-06-30 P0-1 集成测试:验证
//! `Sanitizer::from_config` 的运行时行为 —— 这是 `reflect-exec` 启动
//! 时把 `~/.reflect/config.toml [sanitize]` 段真正接到 queue 脱敏 pass
//! 的"配置 → 行为"映射。
//!
//! 历史实现(`pre-review`)硬编码 `Sanitizer::with_defaults()`,用户的
//! `enabled = false` / `marker = "..."` / `extra_patterns = [...]` 全部
//! 死信。本测试集直接构造 `SanitizeConfig` 各种 case,断言
//! `Sanitizer::from_config` 返回的 sanitizer 行为符合配置意图。
//!
//! 注意:`reflect_config::SanitizeSection` → `reflect_tools::sanitize::SanitizeConfig`
//! 的字段映射在 `reflect-exec/src/lib.rs::build_sanitizer` 完成 ——
//! 那是个 5 行字段透传,本测试不重复验证它。本测试聚焦
//! `Sanitizer::from_config` 自身的运行时语义。
//!
//! 这些测试只 import `reflect_tools::*`,不依赖 `reflect-config` /
//! `reflect-exec`(后者带 tokio runtime + 模型 client,不适合纯单测 sandbox)。

use std::sync::Arc;

use reflect_protocol::{ContentBlock, ToolOutput};
use reflect_tools::{
    SanitizeConfig, SanitizeError, Sanitizer,
    sanitize::{sanitize_block, sanitize_output, sanitize_text},
};
use serde_json::json;

// ── P0-1 测试 ──────────────────────────────────────────────────────────

/// `enabled = false` → 等价 `Sanitizer::disabled()`,任何输入原样返回。
#[test]
fn from_config_enabled_false_yields_disabled_sanitizer() {
    let cfg = SanitizeConfig {
        enabled: Some(false),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).expect("valid config");
    assert!(
        !s.is_enabled(),
        "enabled=false must yield disabled sanitizer"
    );
    let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE password=secret";
    assert_eq!(sanitize_text(raw, &s), raw);
}

/// `marker = "[HIDDEN]"` → 默认 pattern 用 HIDDEN 替换而非 REDACTED。
/// 这是用户在 config 里改 marker 真正生效的语义断言。
#[test]
fn from_config_custom_marker_replaces_marker_in_all_patterns() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        marker: Some("[HIDDEN]".into()),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).expect("valid config");
    assert_eq!(s.marker(), "[HIDDEN]");

    // 简单 KEY_ASSIGN 命中:用自定义 marker
    assert_eq!(sanitize_text("API_KEY=secret", &s), "API_KEY=[HIDDEN]");
    // typed marker:AWS key 命中时拼成 `[HIDDEN:aws_key]`
    assert_eq!(
        sanitize_text("AKIAIOSFODNN7EXAMPLE", &s),
        "[HIDDEN:aws_key]"
    );
    // Bearer 替换:Bearer [HIDDEN](保留前缀)
    assert_eq!(
        sanitize_text("Authorization: Bearer abc.def.ghi", &s),
        "Authorization: Bearer [HIDDEN]"
    );
}

/// `extra_patterns` 用户的正则 + 默认 10-pattern 同时生效。
#[test]
fn from_config_extra_patterns_or_with_defaults() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        extra_patterns: Some(vec![
            r"(?i)\bCUSTOM_SECRET_[A-Z0-9]+\b".into(),
            r"(?i)foobar\s*=\s*\S+".into(),
        ]),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).expect("valid config");
    // 默认 pattern 仍生效:API_KEY 被默认 KEY_ASSIGN 替换
    assert_eq!(sanitize_text("API_KEY=secret", &s), "API_KEY=[REDACTED]");
    // extra pattern 1:整段匹配替换
    assert_eq!(
        sanitize_text("value=CUSTOM_SECRET_FOO123", &s),
        "value=[REDACTED]"
    );
    // extra pattern 2 + KEY_ASSIGN 同时命中,KEY_ASSIGN 先跑(value 没命中 KEY),
    // 但 foobar 会被 extra 1 抢先命中 → 整段 = REDACTED
    let out = sanitize_text("foobar=hunter2", &s);
    assert!(out.contains("[REDACTED]") && !out.contains("hunter2"));
}

/// `disable_default_patterns = true` + 仅有 `extra_patterns` → 默认 pattern
/// 不跑,只跑 extra。
#[test]
fn from_config_disable_default_patterns_runs_only_extras() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        disable_default_patterns: Some(true),
        extra_patterns: Some(vec![r"(?i)\bMY_TOKEN\b".into()]),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).expect("valid config");
    // 默认 AWS pattern 不再脱敏
    assert_eq!(
        sanitize_text("AKIAIOSFODNN7EXAMPLE", &s),
        "AKIAIOSFODNN7EXAMPLE"
    );
    // extra 仍脱敏
    assert_eq!(sanitize_text("MY_TOKEN", &s), "[REDACTED]");
}

/// `extra_patterns[i]` 不是合法 regex → 返回 `SanitizeError::InvalidPattern`,
/// index 与原始字符串都能在错误里看到(便于日志定位)。
#[test]
fn from_config_invalid_extra_pattern_returns_indexed_error() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        extra_patterns: Some(vec![
            r"(?i)\bGOOD\b".into(),
            "[unclosed".into(),
            r"(?i)\bALSO_GOOD\b".into(),
        ]),
        ..Default::default()
    };
    let err = Sanitizer::from_config(&cfg).unwrap_err();
    match err {
        SanitizeError::InvalidPattern {
            index,
            pattern_source,
            ..
        } => {
            assert_eq!(index, 1, "error must point to the failing index");
            assert_eq!(pattern_source, "[unclosed");
        }
    }
}

/// 完全默认 `SanitizeConfig` → 等价 `Sanitizer::with_defaults()`(10 个 pattern + `[REDACTED]`)。
/// 这是用户没写 `[sanitize]` 段时的行为 —— reflect-exec 的 `build_sanitizer`
/// 应当走到这一支。
#[test]
fn from_config_default_matches_with_defaults() {
    let from_cfg = Sanitizer::from_config(&SanitizeConfig::default()).expect("valid config");
    let direct = Sanitizer::with_defaults();
    assert!(from_cfg.is_enabled());
    assert_eq!(from_cfg.marker(), direct.marker());
    assert_eq!(from_cfg.pattern_count(), direct.pattern_count());
    assert_eq!(
        from_cfg.pattern_count(),
        10,
        "must have exactly 10 defaults"
    );
    // 同样的输入产生同样的输出
    let inputs = [
        "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
        "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abc",
        "sk-abcdefghijklmnopqrstuv",
        "postgres://user:pass@host",
    ];
    for input in inputs {
        assert_eq!(
            sanitize_text(input, &from_cfg),
            sanitize_text(input, &direct),
            "default mismatch on {input:?}"
        );
    }
}

/// 端到端 `sanitize_output` + multi-block ContentBlock + 自定义 marker:
/// 验证从配置 → sanitizer → 输出整套链路工作。
#[test]
fn from_config_end_to_end_through_sanitize_output() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        marker: Some("[MASKED]".into()),
        ..Default::default()
    };
    let s = Arc::new(Sanitizer::from_config(&cfg).expect("valid config"));
    let mut output = ToolOutput {
        content: vec![
            ContentBlock::text("hello world"),
            ContentBlock::text("API_KEY=secret"),
            ContentBlock::text("AKIAIOSFODNN7EXAMPLE"),
            ContentBlock::Image {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            },
        ],
        is_error: false,
        metadata: json!({}),
        elapsed_ms: 0,
    };
    sanitize_output(&mut output, &s);
    match &output.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "hello world"),
        _ => panic!(),
    }
    match &output.content[1] {
        ContentBlock::Text { text } => assert_eq!(text, "API_KEY=[MASKED]"),
        _ => panic!(),
    }
    match &output.content[2] {
        ContentBlock::Text { text } => assert_eq!(text, "[MASKED:aws_key]"),
        _ => panic!(),
    }
    // Image 块原样不动
    match &output.content[3] {
        ContentBlock::Image { .. } => {}
        _ => panic!("Image must not be touched"),
    }
}

/// nested `ContentBlock::ToolResult` 递归 sanitize 在自定义 marker 下
/// 仍然正确。
#[test]
fn from_config_recurses_into_tool_result_with_custom_marker() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        marker: Some("[GONE]".into()),
        ..Default::default()
    };
    let s = Arc::new(Sanitizer::from_config(&cfg).expect("valid config"));
    let inner_output = ToolOutput {
        content: vec![ContentBlock::text("PASSWORD=hunter2")],
        is_error: false,
        metadata: json!({}),
        elapsed_ms: 0,
    };
    let mut block = ContentBlock::ToolResult {
        call_id: "c".into(),
        output: inner_output,
    };
    sanitize_block(&mut block, &s);
    match block {
        ContentBlock::ToolResult { output, .. } => match &output.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "PASSWORD=[GONE]"),
            _ => panic!(),
        },
        _ => panic!("expected ToolResult"),
    }
}
