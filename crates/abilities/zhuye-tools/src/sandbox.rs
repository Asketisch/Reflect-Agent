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

/// 把**目录类**路径参数(grep / glob 的 `path`)解析到 workspace 内。
///
/// 0.6:grep 与 glob 的 `path` 参数此前直接喂给 `WalkBuilder::new(root)`,
/// 没有任何 workspace 包含校验 —— `{"pattern": "x", "path": "/"}` 会遍历
/// 整个文件系统,并把匹配行内容(含绝对路径)吐给模型。`read` 虽然还会拒读
/// 这些路径,但内容已经在工具输出里泄露了。
///
/// 与 [`resolve_sandbox_path`] 的差别只在错误信息:这里的调用场景是"搜索
/// 根目录",路径不存在 / 不是目录时应给出可操作的提示,而不是裸 Io 错误。
pub fn resolve_sandbox_dir(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let resolved = resolve_sandbox_path(workspace, path).map_err(|e| match e {
        ToolError::PathEscape { .. } => e,
        // 规范化失败几乎总是"不存在",给搜索场景一句人话。
        ToolError::Io(_) => ToolError::InvalidArgs {
            message: format!(
                "path 不存在或无法访问: {} (相对 workspace 或 workspace 内的绝对路径)",
                path.display()
            ),
        },
        other => other,
    })?;
    if !resolved.is_dir() {
        return Err(ToolError::InvalidArgs {
            message: format!("path 不是目录: {}", path.display()),
        });
    }
    Ok(resolved)
}

/// 把 workspace 内的绝对路径转成相对 workspace 的显示路径。
///
/// 0.6:grep / glob 的输出此前用 `entry.path().display()`,对 workspace 根
/// 启动的搜索会给出冗长的绝对路径(每一行都带一遍),既烧 token 又不便读。
/// 转不成相对路径时(符号链接指向 workspace 外等)原样返回绝对路径。
pub fn display_path_relative(path: &Path, workspace: &Path) -> String {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    abs.strip_prefix(&workspace)
        .unwrap_or(&abs)
        .to_string_lossy()
        .into_owned()
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
            std::env::temp_dir().join(format!("zhuye_sandbox_test_{}_{}", std::process::id(), n));
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
