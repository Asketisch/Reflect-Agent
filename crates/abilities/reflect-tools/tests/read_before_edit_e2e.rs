//! 端到端集成测试:`ReadBeforeEditHook` + 真实 `read`/`write`/`edit` builtin +
//! `ToolExecutionQueue` + `HookEngine` 的完整链路。
//!
//! 测试目标:验证 hook 在真实工具调用 + sanitize + PostToolUse 派发之后,
//! 仍然能正确 mark/forget/check —— 即 metadata 中的 `path` / `mtime_ms`
//! 字段对 hook 可用,且 key 一致性(同 canonical 绝对路径)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use reflect_hooks::FileReadStateTracker;
use reflect_hooks::builtins::{ReadBeforeEditConfig, ReadBeforeEditHook};
use reflect_protocol::{ContentBlock, PermissionMode, ToolOutput};
use reflect_tools::{
    ApprovalGate,
    builtins::{EditTool, NotebookEditTool, ReadTool, WriteTool},
    queue::{ToolCallRequest, ToolExecutionQueue},
    registry::{ToolRegistry, ToolSource},
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tmp_workspace() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("reflect_rbe_e2e_{}_{}", std::process::id(), n));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造一个最小 `ToolExecutionQueue`,带 `read`/`write`/`edit` 三个
/// builtin + 共享的 `ReadBeforeEditHook`。
async fn build_queue(workspace: &std::path::Path) -> ToolExecutionQueue {
    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(ReadTool));
    registry.register_with_source(ToolSource::Builtin, Arc::new(WriteTool));
    registry.register_with_source(ToolSource::Builtin, Arc::new(EditTool));

    let engine = Arc::new(reflect_hooks::HookEngine::new());
    let state: Arc<FileReadStateTracker> = Arc::new(FileReadStateTracker::new());
    engine.register(ReadBeforeEditHook::with_defaults(state));

    let ctx = reflect_tools::ToolContext::for_workspace(workspace);
    ToolExecutionQueue::with_defaults(registry, engine, ctx)
}

/// 用 `is_error` 字段提取首个 result 的状态。
fn first(results: &[reflect_tools::queue::ToolResult]) -> &reflect_tools::queue::ToolResult {
    results.first().expect("at least one result")
}

fn make_call(name: &str, args: serde_json::Value) -> ToolCallRequest {
    ToolCallRequest {
        id: format!("call-{name}"),
        name: name.into(),
        args,
    }
}

/// E2E 1:**happy path** —— read 之后 write 允许。
#[tokio::test]
async fn e2e_read_then_write_allows() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let q = build_queue(&ws).await;

    // 1. 先 read → 打标记
    let r1 = q
        .execute_all(vec![make_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )])
        .await;
    assert!(!first(&r1).is_error, "read 应成功");

    // 2. 再 write → 应放行
    let r2 = q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "world"}),
        )])
        .await;
    assert!(!first(&r2).is_error, "write 应允许,因 read 已 mark");

    // 3. 磁盘内容确认
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "world");
}

/// E2E 2:**未读直接 write** → is_error=true(hook Deny 转)。
#[tokio::test]
async fn e2e_write_without_read_denies() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let q = build_queue(&ws).await;

    // tracker 空 → write 被 Deny
    let r = q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
        )])
        .await;
    let res = first(&r);
    assert!(res.is_error, "write 未 read 应被拒");
    // 磁盘内容未变
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "x");
}

/// E2E 3:**read → 外部 touch → write** → Deny(stale since read)。
#[tokio::test]
async fn e2e_write_after_external_mtime_drift_denies() {
    let ws = tmp_workspace();
    let file = ws.join("a.txt");
    std::fs::write(&file, "x").unwrap();
    let q = build_queue(&ws).await;

    // 1. 先 read → 打标记
    let r1 = q
        .execute_all(vec![make_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )])
        .await;
    assert!(!first(&r1).is_error);

    // 2. 外部 touch mtime +2s
    let canonical = FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();
    let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
    let future = mtime + Duration::from_secs(2);
    filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();

    // 3. 再 write → 应拒绝
    let r2 = q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
        )])
        .await;
    let res = first(&r2);
    assert!(res.is_error, "write after drift 应被拒");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x", "文件未变");
}

/// E2E 4:**read → edit → write** → edit 成功,但后续 write 需重 read。
#[tokio::test]
async fn e2e_edit_success_then_write_requires_re_read() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "alpha beta gamma").unwrap();
    let q = build_queue(&ws).await;

    // 1. 先 read → 打标记
    q.execute_all(vec![make_call(
        "read",
        serde_json::json!({"path": "a.txt"}),
    )])
    .await;

    // 2. edit → 成功(因为 read 已 mark)
    let r_edit = q
        .execute_all(vec![make_call(
            "edit",
            serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}),
        )])
        .await;
    assert!(!first(&r_edit).is_error, "edit 在 read 后应允许");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "alpha BETA gamma"
    );

    // 3. 后续 write → Deny(因为 edit 后 tracker 状态需重 read)
    let r_write = q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "full replace"}),
        )])
        .await;
    let res = first(&r_write);
    assert!(
        res.is_error,
        "write 在 edit 后应被拒(需重 read),实际 is_error={}",
        res.is_error
    );
}

/// E2E 5:**read + Plan mode 写入** → PlanModeGate Deny 优先,本 hook Allow。
#[tokio::test]
async fn e2e_plan_mode_denies_with_plan_reason_not_read() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let q = build_queue(&ws).await;

    // read → 打标记
    q.execute_all(vec![make_call(
        "read",
        serde_json::json!({"path": "a.txt"}),
    )])
    .await;

    // 切到 Plan mode(改 ctx.permission_mode)—— queue 的 base_ctx 是
    // 一次性构造,本测试简化为:另建一个 Plan mode queue。
    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(WriteTool));
    let engine = Arc::new(reflect_hooks::HookEngine::new());
    engine.register(reflect_hooks::builtins::PlanModeGate::default_mode());
    engine.register(ReadBeforeEditHook::with_defaults(Arc::new(
        FileReadStateTracker::new(),
    )));
    let mut ctx = reflect_tools::ToolContext::for_workspace(ws.clone());
    ctx.permission_mode = PermissionMode::Plan;
    let plan_q = ToolExecutionQueue::with_defaults(registry, engine, ctx);

    let r = plan_q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
        )])
        .await;
    assert!(
        first(&r).is_error,
        "Plan mode 下 write 应被 PlanModeGate 拒"
    );
    // PlanModeGate Deny reason 包含 "Plan mode",而非 "never read"。
    let content_str: String = first(&r)
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        content_str.contains("Plan mode") || content_str.contains("plan"),
        "Plan mode 应提供 plan 理由,实际: {content_str}"
    );
}

/// E2E 6:**hook enabled=false** → write 未 read 也允许。
#[tokio::test]
async fn e2e_disabled_hook_allows_write_without_read() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "x").unwrap();

    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(WriteTool));
    let engine = Arc::new(reflect_hooks::HookEngine::new());
    let cfg = ReadBeforeEditConfig {
        enabled: Some(false),
        ..Default::default()
    };
    engine.register(ReadBeforeEditHook::new(
        Arc::new(FileReadStateTracker::new()),
        cfg,
    ));
    let ctx = reflect_tools::ToolContext::for_workspace(ws.clone());
    let q = ToolExecutionQueue::with_defaults(registry, engine, ctx);

    let r = q
        .execute_all(vec![make_call(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
        )])
        .await;
    assert!(!first(&r).is_error, "enabled=false 时未 read 也应允许");
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "y");
}

/// E2E 7:**metadata 契约** —— read 的 ToolOutput.metadata 含 `path`
/// (canonical)与 `mtime_ms`,供 hook PostToolUse 使用。
#[tokio::test]
async fn e2e_read_metadata_shape_for_hook() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "x").unwrap();
    let q = build_queue(&ws).await;
    let r = q
        .execute_all(vec![make_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )])
        .await;
    let res = first(&r);
    assert!(!res.is_error);
    let path = res.metadata.get("path").and_then(|v| v.as_str());
    assert!(path.is_some(), "metadata.path 存在");
    assert!(
        std::path::Path::new(path.unwrap()).is_absolute(),
        "metadata.path 必须是 canonical 绝对路径"
    );
    let mtime = res.metadata.get("mtime_ms").and_then(|v| v.as_u64());
    assert!(mtime.is_some(), "metadata.mtime_ms 存在");
}

/// E2E 8:**notebook_edit 未 read → Deny** —— `notebook_edit` 写子动
/// 作(edit/insert/delete)必须先 read 同 .ipynb,否则 hook Deny。
#[tokio::test]
async fn e2e_notebook_edit_without_read_denies() {
    let ws = tmp_workspace();
    let nb = r#"{
        "cells": [{"cell_type": "code", "source": "1+1"}],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    }"#;
    std::fs::write(ws.join("nb.ipynb"), nb).unwrap();

    // 把 NotebookEditTool 也注册到 queue。
    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(ReadTool));
    registry.register_with_source(ToolSource::Builtin, Arc::new(NotebookEditTool));
    let engine = Arc::new(reflect_hooks::HookEngine::new());
    engine.register(ReadBeforeEditHook::with_defaults(Arc::new(
        FileReadStateTracker::new(),
    )));
    let ctx = reflect_tools::ToolContext::for_workspace(ws.clone());
    let q = ToolExecutionQueue::with_defaults(registry, engine, ctx);

    // 直接 notebook_edit edit(未 read)→ Deny → is_error=true。
    let r = q
        .execute_all(vec![make_call(
            "notebook_edit",
            serde_json::json!({
                "action": "edit", "path": "nb.ipynb",
                "index": 0, "source": "2+2"
            }),
        )])
        .await;
    assert!(
        first(&r).is_error,
        "notebook_edit 未 read 应被 read_before_edit Deny"
    );
    // 磁盘内容未变 —— cell source 仍是 "1+1"。
    let content = std::fs::read_to_string(ws.join("nb.ipynb")).unwrap();
    assert!(content.contains("1+1"), "ipynb 内容未变,实际: {content}");
    assert!(!content.contains("2+2"));
}

/// E2E 9:**read .ipynb → notebook_edit edit → Allow** —— read 之后
/// notebook_edit 写入允许;list/read 子动作不需要 read 即可触发。
#[tokio::test]
async fn e2e_read_notebook_then_notebook_edit_allows() {
    let ws = tmp_workspace();
    let nb = r#"{
        "cells": [{"cell_type": "code", "source": "1+1"}],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    }"#;
    std::fs::write(ws.join("nb.ipynb"), nb).unwrap();

    let registry = Arc::new(ToolRegistry::new());
    registry.register_with_source(ToolSource::Builtin, Arc::new(ReadTool));
    registry.register_with_source(ToolSource::Builtin, Arc::new(NotebookEditTool));
    let engine = Arc::new(reflect_hooks::HookEngine::new());
    engine.register(ReadBeforeEditHook::with_defaults(Arc::new(
        FileReadStateTracker::new(),
    )));
    let ctx = reflect_tools::ToolContext::for_workspace(ws.clone());
    let q = ToolExecutionQueue::with_defaults(registry, engine, ctx);

    // 1. notebook_edit list(只读)未 read → Allow
    let r_list = q
        .execute_all(vec![make_call(
            "notebook_edit",
            serde_json::json!({"action": "list", "path": "nb.ipynb"}),
        )])
        .await;
    assert!(!first(&r_list).is_error, "notebook_edit list(只读)应 Allow");

    // 2. read .ipynb → 打标记
    let r_read = q
        .execute_all(vec![make_call(
            "read",
            serde_json::json!({"path": "nb.ipynb"}),
        )])
        .await;
    assert!(!first(&r_read).is_error);

    // 3. notebook_edit edit(写)→ Allow
    let r_edit = q
        .execute_all(vec![make_call(
            "notebook_edit",
            serde_json::json!({
                "action": "edit", "path": "nb.ipynb",
                "index": 0, "source": "2+2"
            }),
        )])
        .await;
    assert!(
        !first(&r_edit).is_error,
        "read 后 notebook_edit edit 应 Allow,实际: {:?}",
        first(&r_edit).content
    );
    let content = std::fs::read_to_string(ws.join("nb.ipynb")).unwrap();
    assert!(content.contains("2+2"), "ipynb 应被修改");
}

/// E2E 10:**canonical key 契约** —— read 工具在 `metadata.path` 暴露
/// 的 canonical 绝对路径,必须字节相等于 tracker 同一文件的 canonical
/// key(由 `FileReadStateTracker::canonicalize` 产出)。这是 hook
/// `on_post` mark 与 `on_pre` check 同 key 比对的前提;若不一致
/// 会导致 `read → write` 静默 Deny。本测试显式断言这个契约,任何
/// 改动 sandbox 或 canonicalize 的人都会被立即捕捉。
#[tokio::test]
async fn e2e_canonical_key_matches_between_read_and_tracker() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.txt"), "hello").unwrap();
    let q = build_queue(&ws).await;

    // tracker 自己算出的 canonical key
    let tracker_key =
        FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();

    // read 工具 metadata.path
    let r = q
        .execute_all(vec![make_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )])
        .await;
    let res = first(&r);
    assert!(!res.is_error);
    let read_key = res
        .metadata
        .get("path")
        .and_then(|v| v.as_str())
        .expect("metadata.path 存在");

    assert_eq!(
        std::path::Path::new(read_key),
        tracker_key,
        "read 工具的 canonical key 必须等于 FileReadStateTracker::canonicalize 输出"
    );
}

// 抑制 dead code warning。
#[allow(dead_code)]
fn _suppress(_: &Arc<RwLock<()>>, _: &ApprovalGate, _: ToolOutput) {}
