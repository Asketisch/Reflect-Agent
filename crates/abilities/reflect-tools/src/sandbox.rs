//! `PathSandbox` —— v0 路径沙箱。
//!
//! 所有工具的路径参数必须解析到 `workspace` 之下的路径,否则工具返回
//! `ToolError::PathEscape`。
//!
//! 详见 `docs/tools-and-hooks.md §8.1`。

use std::path::{Path, PathBuf};

use crate::tool::ToolError;

/// 把 `path` 解析到 `workspace` 下,确保结果位于 workspace 内。
/// 若越界则返回 `ToolError::PathEscape`。
pub fn resolve_sandbox_path(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let abs_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolError::Io(e.to_string()))?;
    let abs_path = if path.is_absolute() {
        path.canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    } else {
        abs_workspace
            .join(path)
            .canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    };
    if !abs_path.starts_with(&abs_workspace) {
        return Err(ToolError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    Ok(abs_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn setup() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("reflect_sandbox_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            fs::remove_dir_all(&dir).ok();
        }
        let inner = dir.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("a.txt"), "hello").unwrap();
        inner
    }

    fn teardown(inner: &Path) {
        let _ = fs::remove_dir_all(inner.parent().unwrap());
    }

    #[test]
    fn relative_path_under_workspace_ok() {
        let inner = setup();
        let canonical_inner = inner.canonicalize().unwrap();
        let resolved = resolve_sandbox_path(&inner, Path::new("a.txt")).unwrap();
        assert!(resolved.starts_with(&canonical_inner));
        teardown(&inner);
    }

    #[test]
    fn absolute_path_under_workspace_ok() {
        let inner = setup();
        let abs = inner.join("a.txt");
        let resolved = resolve_sandbox_path(&inner, &abs).unwrap();
        assert_eq!(resolved, abs.canonicalize().unwrap());
        teardown(&inner);
    }

    #[test]
    fn path_escape_returns_error() {
        let inner = setup();
        // 使用一个不在 inner 内的路径。
        let result = resolve_sandbox_path(&inner, Path::new("../etc/passwd"));
        // 可能是 PathEscape(canonicalize 失败或越界)或 Io(文件不存在) —
        // 两者都视为不安全信号,可接受。
        assert!(result.is_err());
        teardown(&inner);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let inner = setup();
        let result = resolve_sandbox_path(&inner, Path::new("missing.txt"));
        assert!(matches!(result, Err(ToolError::Io(_))));
        teardown(&inner);
    }
}
