//! 脱敏模块单测。整体迁自原 `sanitize.rs` 内联 `#[cfg(test)] mod tests`。

use super::*;

// ── 每个 pattern 正例 ─────────────────────────────────────────

#[test]
fn key_assign_redacts_key_equals_value() {
    let s = Sanitizer::with_defaults();
    assert_eq!(sanitize_text("API_KEY=secret123", &s), "API_KEY=[REDACTED]");
    assert_eq!(sanitize_text("password=hunter2", &s), "password=[REDACTED]");
    assert_eq!(sanitize_text("TOKEN: abcdefg", &s), "TOKEN:[REDACTED]");
    // 大小写不敏感
    assert_eq!(
        sanitize_text("Password=topsecret", &s),
        "Password=[REDACTED]"
    );
}

#[test]
fn bearer_token_redacts_with_prefix_kept() {
    let s = Sanitizer::with_defaults();
    assert_eq!(
        sanitize_text("Authorization: Bearer eyJabc.def.ghi", &s),
        "Authorization: Bearer [REDACTED]"
    );
}

#[test]
fn aws_access_key_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    // 20 字符 AWS access key id(AKIA + 16 位)。
    assert_eq!(
        sanitize_text("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE", &s),
        "AWS_ACCESS_KEY_ID=[REDACTED:aws_key]"
    );
}

#[test]
fn openai_key_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    let key = "sk-abcdefghijklmnopqrstuv";
    let out = sanitize_text(key, &s);
    assert!(out.contains("[REDACTED:openai_key]"));
    assert!(!out.contains("abcdefghijklmnopqrstuv"));
}

#[test]
fn github_token_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let out = sanitize_text(token, &s);
    assert!(out.contains("[REDACTED:github_token]"));
    assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"));
}

#[test]
fn anthropic_key_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
    let out = sanitize_text(key, &s);
    assert!(out.contains("[REDACTED:anthropic_key]"));
    assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"));
}

#[test]
fn private_key_block_redacts_whole_block() {
    let s = Sanitizer::with_defaults();
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...\n-----END RSA PRIVATE KEY-----";
    let out = sanitize_text(pem, &s);
    assert!(out.contains("[REDACTED:private_key]"));
    assert!(!out.contains("MIIEowIBAAK"));
}

#[test]
fn jwt_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxw";
    let out = sanitize_text(jwt, &s);
    assert!(out.contains("[REDACTED:jwt]"));
    assert!(!out.contains("SflKxw"));
}

#[test]
fn db_url_redacts_preserving_scheme() {
    let s = Sanitizer::with_defaults();
    assert_eq!(
        sanitize_text("postgres://user:pass@localhost/db", &s),
        "postgres://[REDACTED]"
    );
    assert_eq!(
        sanitize_text("mongodb://user:pass@host", &s),
        "mongodb://[REDACTED]"
    );
    assert_eq!(sanitize_text("redis://localhost", &s), "redis://[REDACTED]");
}

#[test]
fn slack_token_redacts_with_type() {
    let s = Sanitizer::with_defaults();
    let tok = "xoxb-1234567890-abcdefghij";
    let out = sanitize_text(tok, &s);
    assert!(out.contains("[REDACTED:slack_token]"));
    assert!(!out.contains("1234567890"));
}

// ── false-positive guards ───────────────────────────────────

#[test]
fn bare_word_password_in_prose_not_redacted() {
    let s = Sanitizer::with_defaults();
    // 不带 `=` / `:` 的「password」字面量不应触发 KEY_ASSIGN。
    let out = sanitize_text("please enter your password above to continue", &s);
    assert!(out.contains("password"));
    assert!(!out.contains("[REDACTED]"));
}

#[test]
fn basic_auth_header_not_redacted_as_bearer() {
    let s = Sanitizer::with_defaults();
    // `Authorization: Basic <base64>` 不应被 BEARER_TOKEN 命中。
    let out = sanitize_text("Authorization: Basic dXNlcjpwYXNz", &s);
    assert!(out.contains("dXNlcjpwYXNz"));
}

#[test]
fn short_circuit_fast_path_skips_regex() {
    let s = Sanitizer::with_defaults();
    let out = sanitize_text("ok", &s);
    assert_eq!(out, "ok");
    let out = sanitize_text("done", &s);
    assert_eq!(out, "done");
}

#[test]
fn short_hex_string_not_redacted_as_github_token() {
    // 32 字符 hex 串不是 GitHub token(GitHub token 是 36+ 字符 base62)。
    let s = Sanitizer::with_defaults();
    let out = sanitize_text("hash=deadbeef0123456789abcdef01234567", &s);
    assert!(!out.contains("[REDACTED:github_token]"));
}

#[test]
fn ssh_public_key_not_redacted() {
    let s = Sanitizer::with_defaults();
    let ssh = "ssh-rsa AAAA... user@host";
    let out = sanitize_text(ssh, &s);
    assert!(out.contains("ssh-rsa"));
    // PRIVATE_KEY_BLOCK 只匹配 PRIVATE KEY,不会命中 ssh-rsa 公钥。
    assert!(!out.contains("[REDACTED:private_key]"));
}

// ── 幂等性 ───────────────────────────────────────────────────

#[test]
fn default_patterns_are_idempotent() {
    let s = Sanitizer::with_defaults();
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
        let once = sanitize_text(raw, &s);
        let twice = sanitize_text(&once, &s);
        assert_eq!(
            once, twice,
            "pattern should be idempotent on input: {raw:?} → first {once:?} → second {twice:?}"
        );
    }
}

#[test]
fn marker_does_not_re_match_after_redaction() {
    let s = Sanitizer::with_defaults();
    // 一旦脱敏完成,marker 文本不应触发任何 pattern 再匹配。
    let marker_only = DEFAULT_MARKER;
    let out = sanitize_text(marker_only, &s);
    assert_eq!(out, marker_only);
}

// ── disabled / 配置 ──────────────────────────────────────────

#[test]
fn disabled_sanitizer_is_noop() {
    let s = Sanitizer::disabled();
    assert!(!s.is_enabled());
    let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
    assert_eq!(sanitize_text(raw, &s), raw);
}

#[test]
fn from_config_with_enabled_false_yields_disabled() {
    let cfg = SanitizeConfig {
        enabled: Some(false),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).unwrap();
    assert!(!s.is_enabled());
}

#[test]
fn from_config_with_custom_marker() {
    let cfg = SanitizeConfig {
        marker: Some("[HIDDEN]".into()),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).unwrap();
    assert_eq!(sanitize_text("API_KEY=secret", &s), "API_KEY=[HIDDEN]");
}

#[test]
fn from_config_with_extra_pattern() {
    let cfg = SanitizeConfig {
        extra_patterns: Some(vec![r"(?i)\bMYTOKEN\b".into()]),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).unwrap();
    // 默认 pattern 仍生效
    assert!(sanitize_text("API_KEY=x", &s).contains("[REDACTED]"));
    // 额外 pattern 也生效
    assert_eq!(sanitize_text("MYTOKEN=abc", &s), "[REDACTED]=abc");
}

#[test]
fn from_config_with_disable_default_patterns() {
    let cfg = SanitizeConfig {
        disable_default_patterns: Some(true),
        extra_patterns: Some(vec![r"(?i)\bFOO\b".into()]),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).unwrap();
    // AWS key 不再脱敏(默认 pattern 已禁)
    assert_eq!(
        sanitize_text("AKIAIOSFODNN7EXAMPLE", &s),
        "AKIAIOSFODNN7EXAMPLE"
    );
    // extra pattern 仍生效
    assert_eq!(
        sanitize_text("hello FOO world", &s),
        "hello [REDACTED] world"
    );
}

#[test]
fn from_config_invalid_extra_pattern_returns_error() {
    let cfg = SanitizeConfig {
        extra_patterns: Some(vec!["[unclosed".into()]),
        ..Default::default()
    };
    let err = Sanitizer::from_config(&cfg).unwrap_err();
    match err {
        SanitizeError::InvalidPattern {
            index,
            pattern_source,
            ..
        } => {
            assert_eq!(index, 0);
            assert_eq!(pattern_source, "[unclosed");
        }
    }
}

#[test]
fn from_config_empty_uses_defaults() {
    let cfg = SanitizeConfig::default();
    let s = Sanitizer::from_config(&cfg).unwrap();
    // 默认 pattern 数应 ≥ 10。
    assert!(s.pattern_count() >= 10);
    assert!(s.is_enabled());
    assert_eq!(s.marker(), DEFAULT_MARKER);
}

/// Review 2026-06-30 P2-7:`extra_patterns = Some(vec![])`(显式空 vec)
/// 与 `extra_patterns = None`(未声明字段)行为一致 —— 不影响默认 pattern
/// 集合,`has_extras` 标志保持 false。
///
/// 实现层:`has_extras = is_some_and(|v| !v.is_empty())`,空 vec 时走 false,
/// short-circuit 仍可用。
#[test]
fn from_config_with_empty_extra_patterns_uses_defaults() {
    let cfg = SanitizeConfig {
        enabled: Some(true),
        extra_patterns: Some(vec![]),
        ..Default::default()
    };
    let s = Sanitizer::from_config(&cfg).unwrap();
    assert_eq!(
        s.pattern_count(),
        10,
        "空 extra_patterns 不应改变默认 10-pattern"
    );
    assert!(s.is_enabled());
    // 默认 pattern 仍生效
    assert_eq!(sanitize_text("API_KEY=x", &s), "API_KEY=[REDACTED]");
    // short-circuit 仍命中(has_extras == false)
    assert_eq!(sanitize_text("ok", &s), "ok");
}

// ── ContentBlock 处理 ─────────────────────────────────────────

#[test]
fn text_block_is_redacted() {
    let s = Sanitizer::with_defaults();
    let mut block = ContentBlock::text("API_KEY=secret");
    sanitize_block(&mut block, &s);
    match block {
        ContentBlock::Text { text } => assert_eq!(text, "API_KEY=[REDACTED]"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn image_block_passes_through_untouched() {
    let s = Sanitizer::with_defaults();
    let mut block = ContentBlock::Image {
        data: vec![0xFF, 0xD8, 0xFF],
        mime_type: "image/jpeg".into(),
    };
    sanitize_block(&mut block, &s);
    match block {
        ContentBlock::Image { data, mime_type } => {
            assert_eq!(data, vec![0xFF, 0xD8, 0xFF]);
            assert_eq!(mime_type, "image/jpeg");
        }
        _ => panic!("expected Image"),
    }
}

#[test]
fn tool_use_block_passes_through() {
    let s = Sanitizer::with_defaults();
    let mut block = ContentBlock::ToolUse {
        id: "call_1".into(),
        name: "bash".into(),
        args: serde_json::json!({"cmd": "echo AKIAIOSFODNN7EXAMPLE"}),
    };
    sanitize_block(&mut block, &s);
    // ToolUse 是请求类载荷,不脱敏 —— 用户消息脱敏是后续 v1.1 范围。
    match block {
        ContentBlock::ToolUse { args, .. } => {
            assert!(args.to_string().contains("AKIAIOSFODNN7EXAMPLE"));
        }
        _ => panic!("expected ToolUse"),
    }
}

#[test]
fn tool_result_block_recurses() {
    let s = Sanitizer::with_defaults();
    let inner_output = ToolOutput {
        content: vec![ContentBlock::text("API_KEY=secret")],
        is_error: false,
        metadata: serde_json::json!({}),
        elapsed_ms: 0,
    };
    let mut block = ContentBlock::ToolResult {
        call_id: "c".into(),
        output: inner_output,
    };
    sanitize_block(&mut block, &s);
    match block {
        ContentBlock::ToolResult { output, .. } => match &output.content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "API_KEY=[REDACTED]");
            }
            _ => panic!("expected inner Text"),
        },
        _ => panic!("expected ToolResult"),
    }
}

#[test]
fn diff_block_is_redacted() {
    let s = Sanitizer::with_defaults();
    let mut block = ContentBlock::Diff {
        unified_diff: "+API_KEY=secret\n-ok".into(),
    };
    sanitize_block(&mut block, &s);
    match block {
        ContentBlock::Diff { unified_diff } => {
            assert!(unified_diff.contains("API_KEY=[REDACTED]"));
            assert!(unified_diff.contains("-ok"));
        }
        _ => panic!("expected Diff"),
    }
}

// ── ToolOutput 整体 ─────────────────────────────────────────

#[test]
fn sanitize_output_handles_multi_block() {
    let s = Sanitizer::with_defaults();
    let mut output = ToolOutput {
        content: vec![
            ContentBlock::text("hello"),
            ContentBlock::text("API_KEY=secret"),
            ContentBlock::Image {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            },
        ],
        is_error: false,
        metadata: serde_json::json!({}),
        elapsed_ms: 0,
    };
    sanitize_output(&mut output, &s);
    match &output.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "hello"),
        _ => panic!(),
    }
    match &output.content[1] {
        ContentBlock::Text { text } => assert_eq!(text, "API_KEY=[REDACTED]"),
        _ => panic!(),
    }
    // Image 未动。
    assert!(matches!(&output.content[2], ContentBlock::Image { .. }));
}

// ── 元信息 ──────────────────────────────────────────────────

#[test]
fn default_patterns_count_is_ten() {
    let s = Sanitizer::with_defaults();
    assert_eq!(s.pattern_count(), 10);
}

/// Review 2026-06-30 P0-2:把 `default_patterns()` 的隐式顺序约束
/// 写进测试,防止后续 PR 调整 pattern 顺序时静默打破语义。
///
/// 关键顺序(详见 `default_patterns` 函数内注释):
/// 1. `KEY_ASSIGN` 必须放最后 —— 否则 `AWS_ACCESS_KEY_ID=AKIA...`
///    会被 KEY_ASSIGN 整段吞成 `[REDACTED:key]`,丢失 type 标识。
/// 2. `ANTHROPIC_KEY` 必须先于 `OPENAI_KEY` —— `sk-ant-...` 是
///    `sk-...` 子集,否则 OPENAI 会先抢跑替换成 `[REDACTED:openai_key]`。
/// 3. 具体 provider pattern (AWS/ANTHROPIC/OPENAI/GITHUB/PRIVATE_KEY/
///    JWT/DB_URL/SLACK/BEARER) 必须在 `KEY_ASSIGN` 之前,否则
///    KEY_ASSIGN 会把它们的字面量提前吞掉。
#[test]
fn default_patterns_order_is_canonical() {
    let patterns = default_patterns();
    let ids: Vec<&'static str> = patterns.iter().map(|p| p.id).collect();

    // 1. KEY_ASSIGN 必须放最后
    assert_eq!(
        ids.last().copied(),
        Some("KEY_ASSIGN"),
        "KEY_ASSIGN must be the last pattern (got order: {ids:?})"
    );

    // 2. ANTHROPIC_KEY 必须在 OPENAI_KEY 之前
    let anthropic_pos = ids
        .iter()
        .position(|&id| id == "ANTHROPIC_KEY")
        .expect("ANTHROPIC_KEY must exist");
    let openai_pos = ids
        .iter()
        .position(|&id| id == "OPENAI_KEY")
        .expect("OPENAI_KEY must exist");
    assert!(
        anthropic_pos < openai_pos,
        "ANTHROPIC_KEY (pos {anthropic_pos}) must precede OPENAI_KEY (pos {openai_pos}); \
         otherwise sk-ant-... gets replaced as [REDACTED:openai_key]"
    );

    // 3. 具体 provider pattern 必须在 KEY_ASSIGN 之前
    let key_assign_pos = anthropic_pos.max(openai_pos);
    for concrete in [
        "AWS_ACCESS_KEY",
        "GITHUB_TOKEN",
        "PRIVATE_KEY_BLOCK",
        "JWT",
        "DB_URL",
        "SLACK_TOKEN",
        "BEARER_TOKEN",
    ] {
        let pos = ids
            .iter()
            .position(|&id| id == concrete)
            .unwrap_or_else(|| panic!("{concrete} must exist in default_patterns"));
        assert!(
            pos < key_assign_pos.max(ids.len() - 1),
            "{concrete} (pos {pos}) must come before KEY_ASSIGN (pos {})",
            ids.len() - 1
        );
    }
}

#[test]
fn short_circuit_skips_all_known_pattern_triggers() {
    let s = Sanitizer::with_defaults();
    // 含数字但不含任何触发字符:short-circuit 路径,不应被任何 pattern 影响。
    let inputs = [
        "loaded 100 lines",
        "no matches found",
        "build succeeded",
        "tests passed: 42",
    ];
    for raw in inputs {
        assert_eq!(
            sanitize_text(raw, &s),
            raw,
            "short-circuit broken on {raw:?}"
        );
    }
}

// ── 复杂场景 ───────────────────────────────────────────────

#[test]
fn multi_secret_in_one_string() {
    let s = Sanitizer::with_defaults();
    let raw = "AWS_KEY=AKIAIOSFODNN7EXAMPLE Authorization: Bearer eyJhbGciOi.body.sig";
    let out = sanitize_text(raw, &s);
    assert!(out.contains("[REDACTED:aws_key]"));
    assert!(out.contains("Bearer [REDACTED]"));
    assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
}

#[test]
fn multiline_tool_output_redacts_per_line() {
    let s = Sanitizer::with_defaults();
    let raw = "line 1\nAPI_KEY=secret\nline 3\nTOKEN=hunter2\nline 5";
    let out = sanitize_text(raw, &s);
    assert_eq!(
        out,
        "line 1\nAPI_KEY=[REDACTED]\nline 3\nTOKEN=[REDACTED]\nline 5"
    );
}
