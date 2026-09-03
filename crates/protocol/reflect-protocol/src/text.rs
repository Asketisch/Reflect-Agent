//! 文本工具:UTF-8 安全的字节级截断。
//!
//! 此前多处用 `String::truncate(字节数)` / `&str[..字节数]` 按**字节**截断,
//! 切点落在多字节字符(中文 / emoji)中间时会 panic(`String::truncate`
//! 有界 panic)或产生 mojibake。全仓统一收敛到本模块:调用方按 `max_bytes`
//! 语义拿到保证落在字符边界的前缀。

/// 返回 `s` 的最长前缀,满足:字节长度 ≤ `max_bytes` 且落在 UTF-8 字符边界。
///
/// `max_bytes >= s.len()` 时原样返回,不做拷贝。
#[inline]
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passthrough() {
        assert_eq!(truncate_utf8("hello", 3), "hel");
        assert_eq!(truncate_utf8("hello", 5), "hello");
        assert_eq!(truncate_utf8("hello", 100), "hello");
    }

    #[test]
    fn multibyte_no_panic_and_keeps_boundary() {
        // 5 个汉字 = 15 字节;切在 8 字节处落在"二"中间,回退到 6 字节。
        let s = "零一二三四";
        let cut = truncate_utf8(s, 8);
        assert_eq!(cut, "零一");
        // 任意切点都不 panic 且结果可整体校验为合法 UTF-8(编译期即保证)。
        for i in 0..s.len() {
            let _ = truncate_utf8(s, i);
        }
    }

    #[test]
    fn emoji_four_byte() {
        let s = "a🎉b"; // 1 + 4 + 1 字节
        assert_eq!(truncate_utf8(s, 3), "a");
        assert_eq!(truncate_utf8(s, 5), "a🎉");
        assert_eq!(truncate_utf8(s, 6), "a🎉b");
    }
}
