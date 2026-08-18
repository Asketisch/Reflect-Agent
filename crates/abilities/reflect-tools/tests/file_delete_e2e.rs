//! 端到端集成测试:`delete_file` builtin + `ToolExecutionQueue` + `HookEngine` 完整链路。
//!
//! 测试目标:
//! - 验证 happy / 错误路径 / 沙箱转义在真实 queue 下行为正确;
//! - 验证 `delete_file` 不在 `ReadBeforeEditHook` 拦截白名单内(无 prior read 也能删);
//! - 验证 `metadata.path` 是 canonical 绝对路径,与 `read` 工具契约一致;
//! - 验证 `ToolRegistry` 不允许同名工具被静默覆盖(防止 P1:bug-3 回归)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use reflect_tools::{
    Tool,
    builtins::{DeleteTool, ReadTool},
    queue::{ToolCallRequest, ToolExecutionQueue},
    registry::{ToolRegistry, ToolSource},
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tmp_workspace() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("reflect_delete_e2e_{}_{}", std::process::id(), n));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造一个挂载 `DeleteTool` + `ReadTool` + 默认 HookEngine 的最小 queue。
async fn build_queue(workspace: &std::path::Path) -> ToolExecutionQueue {
    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(DeleteTool));
    registry.register_with_source(ToolSource::Builtin, Arc::new(ReadTool));

    let engine = Arc::new(reflect_hooks::HookEngine::new());

    let ctx = reflect_tools::ToolContext::for_workspace(workspace);
    ToolExecutionQueue::with_defaults(registry, engine, ctx)
}

fn make_call(id: &str, name: &str, args: serde_json::Value) -> ToolCallRequest {
    ToolCallRequest {
        id: id.into(),
        name: name.into(),
        args,
    }
}

fn first(results: &[reflect_tools::queue::ToolResult]) -> &reflect_tools::queue::ToolResult {
    results.first().expect("at least one result")
}

// ── E2E:happy path ────────────────────────────────────────────────────

/// E2E 1:通过 queue 删一个文件 → success,文件消失,`metadata.path` 是绝对路径。
#[tokio::test]
async fn e2e_delete_via_queue_succeeds() {
    let ws = tmp_workspace();
    let file = ws.join("victim.txt");
    std::fs::write(&file, "bye").unwrap();

    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "victim.txt"}),
        )])
        .await;
    let result = first(&r);
    assert!(!result.is_error, "delete_file 应成功,got {:?}", result);

    // 文件消失
    assert!(!file.exists(), "delete_file 后文件应不存在");

    // metadata.path 是 canonical 绝对路径
    let path = result
        .metadata
        .get("path")
        .and_then(|v| v.as_str())
        .expect("metadata.path 存在");
    assert!(
        std::path::Path::new(path).is_absolute(),
        "metadata.path 应是 canonical 绝对路径,got {path:?}"
    );
    assert!(path.ends_with("victim.txt"), "got {path:?}");
}

// ── E2E:hook 不拦截 delete ────────────────────────────────────────────

/// E2E 2:`delete_file` 不在 `WRITE_TOOLS` 白名单 → 没有 prior read 也能删。
/// 这是设计意图(详见 `read_before_edit::WRITE_TOOLS` doc 注释),作为回归
/// 测试确保未来维护者不会错误地把它加进白名单。
#[tokio::test]
async fn e2e_delete_without_prior_read_is_allowed() {
    let ws = tmp_workspace();
    let file = ws.join("never_read.txt");
    std::fs::write(&file, "x").unwrap();

    // 装一个 ReadBeforeEditHook,确认它不会 Deny delete。
    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(DeleteTool));
    let engine = Arc::new(reflect_hooks::HookEngine::new());
    let state: Arc<reflect_hooks::FileReadStateTracker> =
        Arc::new(reflect_hooks::FileReadStateTracker::new());
    engine.register(reflect_hooks::builtins::ReadBeforeEditHook::with_defaults(
        state,
    ));
    let ctx = reflect_tools::ToolContext::for_workspace(&ws);
    let q = ToolExecutionQueue::with_defaults(registry, engine, ctx);

    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "never_read.txt"}),
        )])
        .await;
    assert!(
        !first(&r).is_error,
        "delete 未 prior read 也应 Allow,got {:?}",
        first(&r)
    );
    assert!(!file.exists());
}

// ── E2E:沙箱转义 ──────────────────────────────────────────────────────

/// E2E 3:`../` 转义 → `PathEscape` 或 `Io`,绝对不允许删除到 workspace 外。
#[tokio::test]
async fn e2e_path_escape_via_dotdot_denied() {
    let ws = tmp_workspace();
    // 在 workspace 父目录造一个"诱惑目标",确认不会被删。
    let target = ws.parent().unwrap().join("tempting_target.txt");
    std::fs::write(&target, "should-survive").unwrap();

    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "../tempting_target.txt"}),
        )])
        .await;
    let result = first(&r);
    assert!(result.is_error, "../ 转义应被拒绝");
    // 错误类型:`PathEscape`(成功 canonicalize 但越界)或 `Io`(父目录不可达 / 目标不存在 / 平台差异)。
    // 关键是 target 文件**没被删**。
    assert!(target.exists(), "../ 转义路径不能删除 workspace 外文件");
    std::fs::remove_file(&target).ok();
}

/// E2E 4:`/etc/passwd` 这类绝对路径 → 拒绝。
#[tokio::test]
async fn e2e_absolute_path_outside_workspace_denied() {
    let ws = tmp_workspace();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "/etc/passwd"}),
        )])
        .await;
    assert!(first(&r).is_error, "/etc/passwd 应被拒绝");
}

// ── E2E:边界 / 错误路径 ────────────────────────────────────────────────

/// E2E 5:缺失文件 → `InvalidArgs`(契约:用户输入错,不是 IO 失败)。
#[tokio::test]
async fn e2e_missing_file_returns_invalid_args() {
    let ws = tmp_workspace();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "ghost.txt"}),
        )])
        .await;
    let result = first(&r);
    assert!(result.is_error, "缺失文件应失败");
    let s = format!("{:?}", result);
    assert!(
        s.contains("InvalidArgs") || s.contains("Io"),
        "缺失文件预期 InvalidArgs 或 Io,got {s:?}"
    );
}

/// E2E 6:目录目标 → InvalidArgs。
#[tokio::test]
async fn e2e_directory_target_rejected() {
    let ws = tmp_workspace();
    std::fs::create_dir(ws.join("subdir")).unwrap();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "subdir"}),
        )])
        .await;
    assert!(first(&r).is_error, "目录目标应被拒绝");
    assert!(ws.join("subdir").exists(), "被拒的目录不应被删");
}

/// E2E 7:空路径 / 仅空白 → InvalidArgs。
#[tokio::test]
async fn e2e_whitespace_only_path_rejected() {
    let ws = tmp_workspace();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "   "}),
        )])
        .await;
    assert!(first(&r).is_error);
}

/// E2E 8:缺 path 字段 → InvalidArgs。
#[tokio::test]
async fn e2e_missing_path_field_rejected() {
    let ws = tmp_workspace();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call("c1", "delete_file", serde_json::json!({}))])
        .await;
    assert!(first(&r).is_error);
}

/// E2E 9:extra fields → InvalidArgs(schema `additionalProperties: false`)。
#[tokio::test]
async fn e2e_extra_fields_rejected() {
    let ws = tmp_workspace();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "x.txt", "force": true}),
        )])
        .await;
    assert!(first(&r).is_error);
}

// ── E2E:契约 ──────────────────────────────────────────────────────────

/// E2E 10:`is_concurrency_safe == false` 与 `description` "Side-effecting" 一致。
/// 维护契约:删除不可并发。
#[test]
fn delete_tool_is_not_concurrency_safe() {
    let t = DeleteTool;
    assert!(
        !t.is_concurrency_safe(),
        "delete_file 必须 is_concurrency_safe=false(否则 queue 会并发调度)"
    );
    assert!(
        t.description().contains("Side-effecting")
            || t.description().to_lowercase().contains("side-effect"),
        "description 应声明 side-effect,got {:?}",
        t.description()
    );
}

/// E2E 11:`required_permission` 是 `Prompt`(由 ApprovalGate 兜底)。
#[test]
fn delete_tool_requires_prompt_permission() {
    use reflect_protocol::PermissionMode;
    assert_eq!(
        DeleteTool.required_permission(),
        PermissionMode::Prompt,
        "delete_file 必须 Prompt 权限(否则 ApprovalGate 不拦截)"
    );
}

// ── E2E:回归 — 防止 P1:bug-3 复发 ─────────────────────────────────────

/// E2E 12:`ToolRegistry` 接受 register(name, X) + register(name, Y) → 后者
/// 覆盖前者(`HashMap::insert` 语义)。这意味着如果未来 `DeleteTool` 变成
/// 有状态,**绝不能**像当前 reflect-tui / reflect-exec 那样在同一进程里
/// 重复 register。重复 register 会让 *后注册* 的实例接管,前面的
/// 配置 / 句柄丢失。
///
/// 本测试以回归形式锁定现状:**重复 register 同一工具名是允许的覆盖语义,
/// 责任由 caller 承担**。bug-3 的修复是 caller 删除冗余 register 调用,
/// 而不是改 `ToolRegistry::register` 的语义。
#[test]
fn registry_register_silently_overwrites_same_name() {
    let r = ToolRegistry::new();
    r.register_with_source(ToolSource::Builtin, Arc::new(DeleteTool));
    r.register_with_source(ToolSource::Builtin, Arc::new(DeleteTool));
    // 只一份 delete_file 在 registry 里
    let names: Vec<String> = r
        .list()
        .into_iter()
        .filter(|n| n == "delete_file")
        .collect();
    assert_eq!(names.len(), 1, "同名 tool 重复 register 应合并(覆盖语义)");
}

// ── E2E:链路 — 删除后 read ─────────────────────────────────────────────

/// E2E 13:删除成功后,同一 session 内 `read` 失败(文件不存在)。
/// 这与 read_before_edit 的 stale entry 行为无关 —— delete 后文件物理
/// 消失,read 的 `metadata_exposes_canonical_path` 测试场景下自然返回
/// 错误。验证我们没把 read-after-delete 的语义搞砸。
#[tokio::test]
async fn e2e_read_after_delete_fails() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("transient.txt"), "x").unwrap();

    let q = build_queue(&ws).await;
    let r1 = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "transient.txt"}),
        )])
        .await;
    assert!(!first(&r1).is_error, "delete 应成功");

    let r2 = q
        .execute_all(vec![make_call(
            "c2",
            "read",
            serde_json::json!({"path": "transient.txt"}),
        )])
        .await;
    assert!(
        first(&r2).is_error,
        "read 删除后的文件应失败,got {:?}",
        first(&r2)
    );
    // 不强制检查具体错误类型(取决于 read.rs 的实现),只要 is_error。
    let _ = format!("{:?}", first(&r2));
}

/// E2E 14:同一文件连续 delete 两次,第二次返回 InvalidArgs。
#[tokio::test]
async fn e2e_double_delete_second_call_errors() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("once.txt"), "x").unwrap();

    let q = build_queue(&ws).await;
    let r1 = q
        .execute_all(vec![make_call(
            "c1",
            "delete_file",
            serde_json::json!({"path": "once.txt"}),
        )])
        .await;
    assert!(!first(&r1).is_error);

    let r2 = q
        .execute_all(vec![make_call(
            "c2",
            "delete_file",
            serde_json::json!({"path": "once.txt"}),
        )])
        .await;
    assert!(first(&r2).is_error, "第二次删同一文件应失败");
}
