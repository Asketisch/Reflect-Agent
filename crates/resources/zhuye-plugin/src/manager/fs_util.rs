//! 工具:递归复制目录(避免引入额外依赖)。

use std::fs;
use std::path::Path;

/// 递归复制整个目录树。`fs::copy` 单文件版,叠加自写遍历。
///
/// 用 `walkdir` 已在 workspace.dependencies,但 v0 简单场景 std 已够用,
/// 避免引入 `dirs` 之类。若未来要支持跨设备,再换 `fs_extra`。
pub(super) fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_symlink() {
            // 跳过 symlink,避免越界与循环;plugin 通常不需要。
            tracing::debug!(path = %from.display(), "copy_dir_all: skipping symlink");
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
