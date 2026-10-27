//! 内部工具 —— 给 fetcher 各实现复用。

use std::path::{Path, PathBuf};

/// 生成 dest 旁边的临时目录路径 —— 用 `.tmp-<rand>` 后缀避免与 dest 同级冲突。
pub(super) fn tmp_sibling(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let stem = dest.file_name().and_then(|s| s.to_str()).unwrap_or("dest");
    // 用 nanosecond 时间戳做后缀;dest 同级冲突概率极低。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!("{stem}.tmp-{nanos}"))
}

/// 截断 stderr 到 `max_bytes`,避免超长输出爆栈。
pub(super) fn truncate_stderr(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        // 找最近的 UTF-8 字符边界,避免截到半截 multi-byte。
        let mut cut = max_bytes;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…(truncated)", &s[..cut])
    }
}
