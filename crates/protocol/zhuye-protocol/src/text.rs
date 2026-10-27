//! `text` —— 字符串安全截断工具。
//!
//! ## 为什么需要(0.8)
//!
//! 仓库里多处按**字节**数截断字符串:
//!
//! ```ignore
//! text.truncate(OUTPUT_LIMIT);          // bash 工具输出
//! &markdown[..MAX_OUTPUT_CHARS]         // web_fetch markdown
//! r.truncate(2000);                     // background_status
//! s.truncate(MAX_JSONL_FIELD_CHARS);    // rollout redact / telemetry
//! ```
//!
//! `String::truncate` 与 byte-slice 索引都要求落在 **char 边界**上,否则
//! panic。CJK 文本里一个汉字占 3 字节,`truncate(100_000)` 恰好切在某个
//! 汉字的第二 / 第三个字节上就会炸 —— 而 `web_fetch` 抓中文页面是高频
//! 场景(本仓库自己的注释就是中文)。
//!
//! 同类 bug 在 `zhuye-exec/src/runtime_config.rs` 已经被修过一次(那里
//! 的 coordinator footer 专门处理了多字节边界),说明这是已知模式,只是漏
//! 了上面几处。Rust 1.85(本仓库 MSRV)没有稳定的
//! `str::floor_char_boundary`,所以在这里自己实现一份,各处共用。

/// 把字节下标 `idx` 向下调整到**不超过它的最近的 char 边界**。
///
/// 语义与 `str::floor_char_boundary`(Rust 1.87 稳定)一致:
/// - `idx >= s.len()` → `s.len()`
/// - `idx` 已是边界 → `idx`
/// - `idx` 落在某个多字节字符中间 → 回退到该字符的起始字节
///
/// 这样 `&s[..floor_char_boundary(s, idx)]` 与 `s.truncate(floor_char_boundary(s, idx))`
/// 都不会 panic。
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    // UTF-8 的续字节是 0b10xx_xxxx;起始字节不是。从 idx 往前找到第一个
    // 非续字节即为字符起点。最多回退 3 字节(UTF-8 最长 4 字节)。
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 把 `s` 截断到至多 `max_bytes` 字节,且不切断任何字符。
///
/// 返回的 `&str` 长度 ≤ `max_bytes`。若 `s` 本身没超长则原样返回。
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[..floor_char_boundary(s, max_bytes)]
}

/// 把 `s` 原地截断到至多 `max_bytes` 字节,不切断任何字符。
///
/// `String::truncate` 的安全替身:`String::truncate` 在 `new_len` 不是
/// char 边界时会 panic,这里保证不会。
pub fn truncate_string_in_place(s: &mut String, max_bytes: usize) {
    let cut = floor_char_boundary(s, max_bytes);
    s.truncate(cut);
}

/// 按**字符数**(而非字节)截断 `s`,返回截断后的 `String`。
///
/// 用于"显示给模型 / 用户的前 N 个字符"这类语义 —— 按字节截断会让
/// 中文内容实际留下的字符数远少于预期(且可能切坏字符)。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_boundary_ascii_unchanged() {
        let s = "hello";
        for i in 0..=s.len() {
            assert_eq!(floor_char_boundary(s, i), i);
        }
        assert_eq!(floor_char_boundary(s, 99), s.len());
    }

    /// 核心回归(0.8):下标落在多字节字符中间必须回退到字符起点。
    /// "中文" = 6 字节,每个汉字 3 字节;下标 1/2 落第一个汉字中间。
    #[test]
    fn floor_boundary_steps_back_into_multibyte_char() {
        let s = "中文";
        assert_eq!(s.len(), 6);
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 0, "下标 1 在第一个汉字中间");
        assert_eq!(floor_char_boundary(s, 2), 0, "下标 2 在第一个汉字中间");
        assert_eq!(floor_char_boundary(s, 3), 3, "下标 3 是第二个汉字起点");
        assert_eq!(floor_char_boundary(s, 4), 3);
        assert_eq!(floor_char_boundary(s, 5), 3);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 100), 6, "越界应钳到 len");
    }

    /// 核心回归(0.8):4 字节字符(emoji)也要正确处理。
    #[test]
    fn floor_boundary_handles_4byte_chars() {
        let s = "a😀b"; // 'a'=1, emoji=4, 'b'=1 → 6 字节
        assert_eq!(s.len(), 6);
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 1);
        assert_eq!(floor_char_boundary(s, 4), 1);
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn floor_boundary_empty_string() {
        assert_eq!(floor_char_boundary("", 0), 0);
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    /// 回归(0.8):`truncate_str` 绝不能 panic,且结果必须是合法 &str。
    /// 对每个可能的字节下标都试一遍 —— 修复前 `&markdown[..N]` 在中日韩
    /// 页面上必炸。
    #[test]
    fn truncate_str_never_panics_on_any_boundary() {
        let s = "中文测试abc😀end";
        for i in 0..=s.len() + 5 {
            let out = truncate_str(s, i);
            assert!(out.len() <= i, "i={i} 得到 len={}", out.len());
            assert!(s.starts_with(out), "截断结果必须是原字符串前缀");
            // 能重新获得 &str 就说明是合法 UTF-8(编译期已保证,这里是双保险)
            let _ = out.to_string();
        }
    }

    #[test]
    fn truncate_string_in_place_never_panics() {
        for i in 0..10 {
            let mut s = String::from("中文测试");
            truncate_string_in_place(&mut s, i);
            assert!(s.len() <= i);
            assert!(s.chars().count() * 3 <= s.len() + 2, "不应切断字符");
        }
    }

    #[test]
    fn truncate_str_passthrough_when_short() {
        assert_eq!(truncate_str("abc", 100), "abc");
        assert_eq!(truncate_str("", 0), "");
    }

    #[test]
    fn truncate_chars_counts_chars_not_bytes() {
        // 按字符:保留前 2 个汉字;按字节的话 2 字节连一个汉字都放不下。
        assert_eq!(truncate_chars("中文测试", 2), "中文");
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("😀😀", 1), "😀");
    }
}
