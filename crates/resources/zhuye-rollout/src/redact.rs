//! 在每条 [`crate::RolloutRecord`] 落盘前对其执行的脱敏 pass。
//!
//! 两层 pass 在每个 `String` 叶子字段上按序执行:
//!
//! 1. **密钥 pattern 脱敏**:通过 [`zhuye_sanitize::redact_string`]
//!    (10 类默认 pattern:AWS / OpenAI / GitHub / Anthropic / PEM / JWT /
//!    DB URLs / Slack / Bearer / `KEY=` 赋值)。幂等 —— `[REDACTED]` 标记
//!    不会再次命中。这是合规边界:缺失此层,用户 prompt 或工具 payload
//!    中的密钥会原样落到磁盘 JSONL transcript。
//! 2. **尺寸截断**:任何长度超过
//!    [`crate::types::MAX_JSONL_FIELD_CHARS`] 的 `String` 被截断,末尾
//!    追加 [`crate::types::JSONL_REDACTION_MARKER`]。
//!
//! 两层合在一起的 pass 是幂等的(密钥 marker 内不含密钥字面量,尺寸
//! marker 在密钥脱敏之后应用)。

use crate::types::{JSONL_REDACTION_MARKER, MAX_JSONL_FIELD_CHARS};
use zhuye_sanitize::redact_string;

/// 就地脱敏 JSON value。返回同一引用,便于链式调用。
///
/// v1.x:降为 `pub(crate)` —— 此前 `pub` 但无 crate 外调用者(zhuye-telemetry
/// 有独立同名实现)。对外入口是 `serialize_redacted`(writer 用)。
pub(crate) fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            // 1. 密钥 pattern 脱敏(合规边界)。
            *s = redact_string(s);
            // 2. 尺寸截断。
            // 0.8:`String::truncate` 在非 char 边界会 panic —— JSONL 里
            // 存的是用户 prompt / 工具输出,CJK 内容极常见,16384 的固定
            // 字节偏移很容易切在汉字中间。改用协议层的安全截断。
            if s.len() > MAX_JSONL_FIELD_CHARS {
                let cut = MAX_JSONL_FIELD_CHARS.saturating_sub(JSONL_REDACTION_MARKER.len());
                // `truncate_str` 返回 &str,而 `s` 正被借用 → 先算长度再
                // 一次性重建,避免 `truncate(0)` + `push_str` 的双次拷贝。
                let kept_len = zhuye_protocol::truncate_str(s, cut).len();
                s.truncate(kept_len);
                s.push_str(JSONL_REDACTION_MARKER);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                redact_value(val);
            }
        }
        _ => {}
    }
}

/// 便捷函数:对一条 record 先脱敏再序列化为单行 JSONL。
pub fn serialize_redacted(r: &crate::RolloutRecord) -> serde_json::Result<String> {
    let mut v = serde_json::to_value(r)?;
    redact_value(&mut v);
    serde_json::to_string(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RolloutRecord;
    use zhuye_protocol::{MessageRole, ThreadId, TurnId};

    #[test]
    fn truncates_strings_over_16kb() {
        let mut v = serde_json::json!("x".repeat(20_000));
        redact_value(&mut v);
        let s = v.as_str().unwrap();
        assert!(s.ends_with(JSONL_REDACTION_MARKER));
        assert!(s.len() <= MAX_JSONL_FIELD_CHARS + JSONL_REDACTION_MARKER.len());
    }

    #[test]
    fn preserves_strings_under_16kb() {
        let mut v = serde_json::json!("hello world");
        redact_value(&mut v);
        assert_eq!(v, serde_json::json!("hello world"));
    }

    #[test]
    fn recurses_into_arrays_and_objects() {
        let mut v = serde_json::json!({
            "small": "ok",
            "big": "y".repeat(20_000),
            "nested": {
                "inner": "z".repeat(20_000),
            },
            "list": ["a".repeat(20_000), "short"],
        });
        redact_value(&mut v);
        assert_eq!(v["small"], "ok");
        assert_eq!(v["list"][1], "short");
        assert!(v["big"].as_str().unwrap().ends_with(JSONL_REDACTION_MARKER));
        assert!(
            v["nested"]["inner"]
                .as_str()
                .unwrap()
                .ends_with(JSONL_REDACTION_MARKER)
        );
        assert!(
            v["list"][0]
                .as_str()
                .unwrap()
                .ends_with(JSONL_REDACTION_MARKER)
        );
    }

    #[test]
    fn idempotent() {
        let mut v = serde_json::json!("x".repeat(20_000));
        redact_value(&mut v);
        let once = v.clone();
        redact_value(&mut v);
        assert_eq!(v, once);
    }

    #[test]
    fn serialize_redacted_marks_large_message_content() {
        let r = RolloutRecord::message(
            TurnId::new(),
            MessageRole::Assistant,
            serde_json::json!("y".repeat(20_000)),
        );
        let line = serialize_redacted(&r).unwrap();
        assert!(
            line.contains(JSONL_REDACTION_MARKER),
            "missing marker in {line}"
        );
        assert!(
            line.len() <= MAX_JSONL_FIELD_CHARS + 200,
            "line too long: {}",
            line.len()
        );
        let _back: RolloutRecord = serde_json::from_str(&line).unwrap();
    }

    #[test]
    fn _session_meta_roundtrips() {
        let sid = ThreadId::new();
        let r = RolloutRecord::session_meta(sid, "openai/gpt-4o");
        let line = serialize_redacted(&r).unwrap();
        let back: RolloutRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
    }

    // ── C1: 密钥 pattern 脱敏(合规边界) ─────────────────────────

    #[test]
    fn redacts_aws_key_in_message_content() {
        let r = RolloutRecord::message(
            TurnId::new(),
            MessageRole::User,
            serde_json::json!("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"),
        );
        let line = serialize_redacted(&r).unwrap();
        assert!(
            !line.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked: {line}"
        );
        assert!(
            line.contains("[REDACTED:aws_key]"),
            "missing redaction marker"
        );
        // round-trip 仍可解析
        let _back: RolloutRecord = serde_json::from_str(&line).unwrap();
    }

    #[test]
    fn redacts_github_token_in_nested_json() {
        let mut v = serde_json::json!({
            "tool": "bash",
            "args": { "cmd": "echo ghp_abcdefghijklmnopqrstuvwxyz0123456789" },
            "output": "token is ghp_abcdefghijklmnopqrstuvwxyz0123456789 here"
        });
        redact_value(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("ghp_abcdefghijklmnopqrstuvwxyz"),
            "token leaked"
        );
        assert!(s.contains("[REDACTED:github_token]"));
    }

    #[test]
    fn redacts_bearer_in_message_and_preserves_prefix() {
        let r = RolloutRecord::message(
            TurnId::new(),
            MessageRole::Assistant,
            serde_json::json!("Authorization: Bearer eyJabc.def.ghi"),
        );
        let line = serialize_redacted(&r).unwrap();
        assert!(!line.contains("eyJabc.def.ghi"));
        assert!(line.contains("Bearer [REDACTED]"));
    }

    #[test]
    fn redaction_idempotent_with_size_truncation() {
        // 密钥脱敏后再走 size 截断,二次调用结果不变。
        let mut v = serde_json::json!("TOKEN=AKIAIOSFODNN7EXAMPLE");
        redact_value(&mut v);
        let once = v.clone();
        redact_value(&mut v);
        assert_eq!(v, once);
    }
    /// 回归(0.8):CJK 内容超长时不得因切在汉字中间而 panic。
    /// 修复前 `s.truncate(cut)` 在 cut 不是 char 边界时直接 panic。
    #[test]
    fn truncation_survives_multibyte_boundary() {
        // 一个汉字 3 字节。构造长度远超上限的纯中文串。
        let cjk = "中".repeat(MAX_JSONL_FIELD_CHARS * 2);
        let mut v = serde_json::Value::String(cjk);
        redact_value(&mut v);
        let s = v.as_str().unwrap();
        assert!(
            s.len() <= MAX_JSONL_FIELD_CHARS,
            "截断后应不超过上限,得到 {}",
            s.len()
        );
        assert!(s.ends_with(JSONL_REDACTION_MARKER), "应带截断标记");
        // 中文被正确保留(不是乱码 / 空串)。
        assert!(s.contains('中'), "中文内容应完整保留");
    }

    /// 回归(0.8):emoji(4 字节)同样不得 panic。
    #[test]
    fn truncation_survives_4byte_boundary() {
        let emoji = "\u{1F600}".repeat(MAX_JSONL_FIELD_CHARS * 2);
        let mut v = serde_json::Value::String(emoji);
        redact_value(&mut v);
        assert!(v.as_str().unwrap().len() <= MAX_JSONL_FIELD_CHARS);
    }

    /// 回归:未超长的中文串必须原样保留(不被误截)。
    #[test]
    fn short_multibyte_untouched() {
        let mut v = serde_json::Value::String("中文测试".into());
        redact_value(&mut v);
        assert_eq!(v.as_str().unwrap(), "中文测试");
    }
}
