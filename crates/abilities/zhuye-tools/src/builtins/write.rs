//! `write` —— 写文本文件(自动 mkdir,返回 unified diff)。
//!
//! 详见 `docs/tools-and-hooks.md §2.3`。

use std::fs;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::Value;
use similar::TextDiff;
use tracing::warn;
use zhuye_protocol::PermissionMode;

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.write")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace, or absolute path under workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Full new file content (overwrites existing file)"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                // 错误信息保留关键字 `missing 'content'` 兼容已有测试,
                // 同时给 LLM 一条清晰路径:Plan mode 下写 plan 应改用 PlanWrite。
                message: "missing 'content' — write 需要 path 与 content 两个参数。Plan mode 下写 plan 推荐改用 PlanWrite {name, content}".into(),
            })?;

        let path = std::path::Path::new(path_str);
        // 计算最终路径:workspace 与 path 按 component 逐级拼接。
        // 文件(或其父目录)可能还不存在,所以先把现存的最深祖先 canonicalize,
        // 再拼上后续 component。
        //
        // **0.2 修复(安全)**:workspace 包含校验此前被 `if path.is_absolute()`
        // 包着,相对路径完全不查 —— 而 component-walk 里 `PathBuf::push("..")`
        // 是真实生效的(`ws/a/../../etc/passwd` 会一步步 canonicalize 到
        // workspace 外),于是 `{"path": "../../etc/passwd"}` 能写到
        // workspace 之外。原注释声称「按 component 增量 canonicalize,这一步
        // 隐式完成」是错的:canonicalize 只解析路径,不做任何包含校验。
        // 现在改为**无条件**校验最终路径,与绝对 / 相对无关。
        let canonical_workspace = ctx
            .workspace_path()
            .canonicalize()
            .map_err(|e| ToolError::Io(format!("workspace: {e}")))?;
        let mut final_path = canonical_workspace.clone();
        for component in path.components() {
            final_path.push(component);
            if final_path.exists() {
                if let Ok(c) = final_path.canonicalize() {
                    final_path = c;
                }
            }
            // 否则保持原样,后续会创建它(或它的父目录)。
        }
        // 沙箱校验:最终路径(无论输入是绝对还是相对)必须在 workspace 内。
        // `starts_with` 按 path component 比较,不会把 `/ws-foo` 误判为
        // `/ws` 的子路径。
        if !final_path.starts_with(&canonical_workspace) {
            warn!(
                path = path_str,
                resolved = %final_path.display(),
                "write: path escapes workspace, rejecting"
            );
            return Err(ToolError::PathEscape {
                path: path.to_path_buf(),
            });
        }
        // 目标不允许是已存在目录(会把内容写进目录导致 Io 错误,报清楚点)。
        if final_path.is_dir() {
            return Err(ToolError::InvalidArgs {
                message: format!("path 是目录,不能当文件写: {path_str}"),
            });
        }

        // 自动 mkdir。
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ToolError::Io(format!("mkdir {parent:?}: {e}")))?;
        }
        let before = fs::read_to_string(&final_path).unwrap_or_default();
        let new_owned = content.to_string();
        let unified = {
            let diff = TextDiff::from_lines(&before, &new_owned);
            diff.unified_diff().to_string()
        };

        // 原子落盘:先写同目录 .tmp 再 rename。`fs::write` 直接截断目标,
        // 进程在写入中途崩溃会把原文件留成半截内容(数据损坏)。
        // rename 在同一文件系统内是原子的,且 .tmp 与目标同目录以保证
        // 这一点(跨设备 rename 会退化为拷贝,失去原子性)。
        let tmp_path = final_path.with_extension({
            let mut ext = final_path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !ext.is_empty() {
                ext.push('.');
            }
            ext.push_str("zhuye-tmp");
            ext
        });
        fs::write(&tmp_path, content)
            .map_err(|e| ToolError::Io(format!("write {path_str}: {e}")))?;
        if let Err(e) = fs::rename(&tmp_path, &final_path) {
            // rename 失败时清掉临时文件,别在 workspace 里留垃圾。
            let _ = fs::remove_file(&tmp_path);
            return Err(ToolError::Io(format!("rename into {path_str}: {e}")));
        }

        Ok(ToolOutput {
            content: vec![
                zhuye_protocol::ContentBlock::text(format!(
                    "wrote {path_str} ({} bytes)",
                    content.len()
                )),
                zhuye_protocol::ContentBlock::Diff {
                    unified_diff: unified.clone(),
                },
            ],
            is_error: false,
            metadata: serde_json::json!({
                "path": final_path.to_string_lossy(),
                "path_str": path_str,
                "bytes": content.len(),
                "diff": unified,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_ctx(workspace: &std::path::Path) -> ToolContext {
        ToolContext::for_workspace(workspace)
    }

    fn tmp_workspace() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("zhuye_write_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn writes_new_file() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "hello\nworld"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "hello\nworld"
        );
        // Diff 块应存在。
        assert!(
            out.content
                .iter()
                .any(|c| matches!(c, zhuye_protocol::ContentBlock::Diff { .. }))
        );
    }

    #[tokio::test]
    async fn auto_creates_parent_dirs() {
        let ws = tmp_workspace();
        let ws_canonical = ws.canonicalize().unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/dir/a.txt", "content": "x"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "execute failed: {out:?}");
        assert!(ws_canonical.join("sub/dir/a.txt").exists());
    }

    #[tokio::test]
    async fn overwrites_existing_file_with_diff() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "old\n").unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "new\n"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "new\n");
        // Diff 应包含删除 / 添加标记。
        let diff = out.content.iter().find_map(|c| match c {
            zhuye_protocol::ContentBlock::Diff { unified_diff } => Some(unified_diff.clone()),
            _ => None,
        });
        let diff = diff.expect("diff block");
        assert!(diff.contains("-old") || diff.contains("---"));
        assert!(diff.contains("+new") || diff.contains("+++"));
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    // ── 0.2 回归:相对路径不得穿越 workspace ────────────────────────

    /// 回归(0.2):`write` 的沙箱校验此前被 `if path.is_absolute()` 包着,
    /// 相对路径完全不查。而 component-walk 里 `push("..")` 真实生效,
    /// 所以 `../../etc/passwd` 能写到 workspace 外。
    #[tokio::test]
    async fn rejects_relative_parent_dir_escape() {
        let ws = tmp_workspace();
        let outside = ws.parent().unwrap().join("zhuye_write_escape_probe.txt");
        let _ = std::fs::remove_file(&outside);
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "../zhuye_write_escape_probe.txt", "content": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathEscape { .. }),
            "相对 .. 穿越必须被拒,得到: {err:?}"
        );
        assert!(
            !outside.exists(),
            "workspace 外不得被写入: {}",
            outside.display()
        );
    }

    /// 回归(0.2):嵌套 `..`(先下钻再跳出)同样必须被拒。
    #[tokio::test]
    async fn rejects_nested_parent_dir_escape() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let outside = ws.parent().unwrap().join("zhuye_write_escape_probe2.txt");
        let _ = std::fs::remove_file(&outside);
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/../../zhuye_write_escape_probe2.txt", "content": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathEscape { .. }),
            "嵌套 .. 穿越必须被拒,得到: {err:?}"
        );
        assert!(!outside.exists());
    }

    /// 回归(0.2):绝对路径穿越 workspace 仍须被拒(原有行为,防回归)。
    #[tokio::test]
    async fn rejects_absolute_path_escape() {
        let ws = tmp_workspace();
        let outside = ws.parent().unwrap().join("zhuye_write_escape_probe3.txt");
        let _ = std::fs::remove_file(&outside);
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": outside.to_string_lossy(), "content": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathEscape { .. }),
            "绝对路径穿越必须被拒,得到: {err:?}"
        );
        assert!(!outside.exists());
    }

    /// 回归(0.2):指向 workspace 外**已存在**文件的 symlink 必须被拒。
    /// component-walk 对每个现存 component 做 canonicalize,所以这条
    /// symlink 会被解析到 workspace 外的真实路径,被最终校验拦下。
    #[tokio::test]
    async fn rejects_symlink_to_existing_outside_file() {
        let ws = tmp_workspace();
        let outside_dir = ws.parent().unwrap().join("zhuye_write_link_target");
        std::fs::create_dir_all(&outside_dir).ok();
        let outside_file = outside_dir.join("exists.txt");
        std::fs::write(&outside_file, "original").unwrap();
        #[cfg(not(unix))]
        {
            // 非 Unix(Windows):创建 symlink 需要管理员/开发者模式,
            // 本回归仅在 Unix 上执行。
            let _ = outside_file;
            return;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_file, ws.join("link.txt")).unwrap();
            let t = WriteTool;
            let err = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "link.txt", "content": "pwned"}),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::PathEscape { .. }),
                "指向外部已存在文件的 symlink 必须被拒,得到: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&outside_file).unwrap(),
                "original",
                "symlink 目标内容不得被改写"
            );
        }
    }

    /// 回归(0.2):**悬空** symlink(target 不存在)不得把文件写到 workspace 外。
    ///
    /// 这条依赖 temp+rename 原子落盘:`fs::write` 会跟随悬空 symlink 在
    /// workspace 外创建目标文件(旧实现的逃逸路径),而 `rename` 是**替换**
    /// symlink 本身,写入落在 workspace 内。因此这里不强制要求报错,只
    /// 断言 workspace 外不得出现新文件。
    #[tokio::test]
    async fn dangling_symlink_does_not_write_outside() {
        let ws = tmp_workspace();
        let outside_file = ws.parent().unwrap().join("zhuye_write_dangling_target.txt");
        let _ = std::fs::remove_file(&outside_file);
        #[cfg(not(unix))]
        {
            // 非 Unix(Windows):创建 symlink 需要管理员/开发者模式,
            // 本回归仅在 Unix 上执行。
            let _ = outside_file;
            return;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_file, ws.join("dangling.txt")).unwrap();
            let t = WriteTool;
            let out = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "dangling.txt", "content": "pwned"}),
                )
                .await;
            assert!(
                !outside_file.exists(),
                "悬空 symlink 不得导致 workspace 外被写入: {}",
                outside_file.display()
            );
            // 落盘成功时,workspace 内的 symlink 已被替换为普通文件。
            if out.is_ok() {
                assert!(ws.join("dangling.txt").is_file());
                assert!(
                    !std::fs::symlink_metadata(ws.join("dangling.txt"))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
        }
    }

    /// 回归(0.2):workspace 内的正常相对路径仍须放行(防过度收紧)。
    #[tokio::test]
    async fn still_allows_in_workspace_relative_path() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/deep/a.txt", "content": "ok"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "workspace 内相对路径应放行: {out:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("sub/deep/a.txt")).unwrap(),
            "ok"
        );
    }

    /// 回归:目录目标应给出清晰错误而非 Io 噪声。
    #[tokio::test]
    async fn rejects_directory_target() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("adir")).unwrap();
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "adir", "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { .. }),
            "目录目标应报 InvalidArgs,得到: {err:?}"
        );
    }

    /// 回归:原子落盘不得在 workspace 里留下 .zhuye-tmp 残留。
    #[tokio::test]
    async fn leaves_no_temp_file_behind() {
        let ws = tmp_workspace();
        let t = WriteTool;
        t.execute(
            make_ctx(&ws),
            serde_json::json!({"path": "a.txt", "content": "hello"}),
        )
        .await
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&ws)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("zhuye-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不得残留临时文件: {leftovers:?}");
    }
}
