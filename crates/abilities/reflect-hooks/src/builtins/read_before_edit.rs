//! `read_before_edit` — PreToolUse hook:deny `write`/`edit`/
//! `notebook_edit`(写子动作)除非本 session 内 agent 已 `read` 过目标文件。
//!
//! 配套 [`PlanModeGate`](crate::builtins::plan_mode_gate::PlanModeGate):
//! - Plan mode 下 gate **早退 Allow**(PlanModeGate 已 Deny,避免双重
//!   Deny + reason 混淆);
//! - 非 Plan mode 下拦截 `WRITE_TOOLS` 白名单内工具的写意图
//!   (只读工具天然不受影响,详见 `is_write_intent`)。
//!
//! ## bash 绕过路径(已知)
//!
//! v1 **不**拦截 `bash` 工具 —— shell 解析代价高(`shell-words` /
//! tree-sitter-bash 路线)。agent 可通过 `echo > foo.txt` / `sed -i` /
//! `tee` 绕过本 hook。这是设计权衡:本 hook 是"防误覆盖"层而非安全
//! 边界,真正的安全层仍是 `PermissionMode::Prompt` + `ApprovalGate`。
//! 详见 plan §R2。
//!
//! ## mtime 容差
//!
//! 漂移阈值 `mtime_drift_tolerance_ms` 默认 500ms(plan R4)——
//! 兼容 macOS HFS+(ns 精度)与 FAT32(2s 精度)。
//!
//! ## 状态
//!
//! 通过 [`SharedFileReadState`](crate::SharedFileReadState) (`Arc`)
//! 与 `read` tool 共享同一 tracker —— `read` 成功时 mark,`write`/
//! `edit` 成功时 forget。详见 `file_read_state.rs`。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use reflect_protocol::PermissionMode;

use crate::decision::HookDecision;
use crate::event::{HookEvent, HookEventKind};
use crate::file_read_state::{FileReadStateTracker, SharedFileReadState};
use crate::hook::Hook;

/// TOML 配置镜像:在 `ReadBeforeEditSection`(`reflect-config::schema`)
/// 之上,这里定义 hook 直接消费的形式。
///
/// 两个字段都 `Option<_>`,提供默认值:
/// - `enabled` 默认 `true`(master switch);
/// - `mtime_drift_tolerance_ms` 默认 `500`。
#[derive(Debug, Clone, Deserialize)]
pub struct ReadBeforeEditConfig {
    pub enabled: Option<bool>,
    pub mtime_drift_tolerance_ms: Option<u64>,
}

impl Default for ReadBeforeEditConfig {
    fn default() -> Self {
        Self {
            enabled: Some(true),
            mtime_drift_tolerance_ms: Some(500),
        }
    }
}

impl ReadBeforeEditConfig {
    /// 解出 `enabled`(`None` → `true`)。
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// 解出 `mtime_drift_tolerance_ms`(`None` → `500`)。
    pub fn drift_tolerance_ms(&self) -> u64 {
        self.mtime_drift_tolerance_ms.unwrap_or(500)
    }
}

/// PreToolUse hook 拦截的工具白名单 —— 只覆盖**会覆盖现有文件内容**
/// 的写工具。删除类工具(`delete_file`)故意不加入,理由:
///
/// 1. 删除不依赖文件内容知识(不需要 prior read 来"知道文件里有什么");
/// 2. 删除由 `PermissionMode::Prompt` + `ApprovalGate` 兜底,不会静默误删;
/// 3. 文件消失后 `FileReadStateTracker` 中的 stale entry 自然失效,
///    无需 PostToolUse forget 显式通知。
///
/// 新增**覆盖式**写工具时请:(1) 加入此列表;(2) 在 `is_write_intent`
/// 处理子动作区分(如 `notebook_edit` 的 list/read vs edit/insert/delete);
/// (3) 在 `forget_target_tools` 处理 PostToolUse forget;(4) 添加
/// e2e 测试覆盖新工具。**删除类工具**不需要任何上述步骤。
pub const WRITE_TOOLS: &[&str] = &["write", "edit", "notebook_edit"];

/// PreToolUse hook:拦截白名单内工具的写入,要求本 session 内已 `read`。
///
/// 由 `reflect-exec::bootstrap_m4` 默认注册;`/hooks disable read_before_edit`
/// 可临时关闭(不反注册),`enabled = false` 配置也可独立关闭。
pub struct ReadBeforeEditHook {
    state: SharedFileReadState,
    config: ReadBeforeEditConfig,
}

impl ReadBeforeEditHook {
    /// 构造 hook。`state` 通常来自 `bootstrap` 与 `read` tool 共享的 `Arc`。
    pub fn new(state: SharedFileReadState, config: ReadBeforeEditConfig) -> Self {
        Self { state, config }
    }

    /// 用默认配置(enabled=true, drift=500ms)构造。
    pub fn with_defaults(state: SharedFileReadState) -> Self {
        Self::new(state, ReadBeforeEditConfig::default())
    }

    /// 从 args 提取 `path` 字段。返回 `None` 表示缺字段(工具层会
    /// 用 `InvalidArgs` 拒绝,hook 让它通过避免重复错误信息)。
    fn extract_path(args: &serde_json::Value) -> Option<&str> {
        args.get("path").and_then(|v| v.as_str())
    }

    /// 判定本次 `tool + args` 是否构成「会修改磁盘文件」的意图。Read-only
    /// 子动作(如 `notebook_edit` 的 `list` / `read`)天然安全,不拦截。
    fn is_write_intent(tool: &str, args: &serde_json::Value) -> bool {
        match tool {
            "write" | "edit" => true,
            "notebook_edit" => {
                // 只拦截会写盘的子动作:edit / insert / delete。
                // list / read 走 `load_notebook` 后只渲染,不调 `save_notebook`。
                matches!(
                    args.get("action").and_then(|v| v.as_str()),
                    Some("edit") | Some("insert") | Some("delete")
                )
            }
            _ => false,
        }
    }

    /// PreToolUse:写入工具 + 未 read → Deny。
    async fn on_pre(&self, event: &HookEvent) -> HookDecision {
        let HookEvent::PreToolUse { tool, args, ctx } = event else {
            return HookDecision::Allow;
        };

        // 1. master switch
        if !self.config.enabled() {
            return HookDecision::Allow;
        }
        // 2. Plan mode 早退 —— PlanModeGate 已 Deny 写工具,不要叠加
        //    第二条 Deny 让 agent 困惑。
        if ctx.permission_mode == PermissionMode::Plan {
            return HookDecision::Allow;
        }
        // 3. 仅拦截白名单内工具的写入意图(白名单见 `WRITE_TOOLS`)。
        if !Self::is_write_intent(tool.as_str(), args) {
            return HookDecision::Allow;
        }
        // 4. 缺 path 字段 —— 让工具层拒绝(避免重复错误信息)。
        let Some(path_str) = Self::extract_path(args) else {
            return HookDecision::Allow;
        };
        // 5. canonicalize 出绝对路径。失败 → Deny(workspace 不可访问
        //    是异常状态,保守拒绝)。
        let path = std::path::Path::new(path_str);
        let canonical = match FileReadStateTracker::canonicalize(&ctx.workspace, path) {
            Ok(p) => p,
            Err(e) => {
                return HookDecision::Deny {
                    reason: format!(
                        "edit denied: cannot resolve '{path_str}' under workspace '{}': {e}",
                        ctx.workspace.display()
                    ),
                };
            }
        };
        // 6. 状态查询
        match self
            .state
            .check_write_safe(&canonical, self.config.drift_tolerance_ms())
        {
            Ok(()) => HookDecision::Allow,
            Err(reason) => HookDecision::Deny {
                reason: reason.into_reason_string(),
            },
        }
    }

    /// PostToolUse:read 成功 → mark;WRITE_TOOLS 内写入成功 → forget(防止
    /// 陈旧 read 记录误导下次校验)。
    async fn on_post(&self, event: &HookEvent) -> HookDecision {
        let HookEvent::PostToolUse { tool, result, .. } = event else {
            return HookDecision::Allow;
        };
        // 失败结果不参与(read 失败 → 没有 mtime 信号,不该 mark;
        // write/edit 失败 → 文件未变,不该 forget)。
        if result.is_error {
            return HookDecision::Allow;
        }
        match tool.as_str() {
            "read" => {
                if let Some(path_str) = result.metadata.get("path").and_then(|v| v.as_str()) {
                    let path = PathBuf::from(path_str);
                    // 优先从 metadata 取 mtime_ms(Phase 4 会注入);缺失则
                    // 退化为 tombstone(read 成功但 mtime 未知 —— 阻止
                    // 后续 write,直到显式 read 拿到 mtime)。
                    let mtime = result
                        .metadata
                        .get("mtime_ms")
                        .and_then(|v| v.as_u64())
                        .and_then(|ms| {
                            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms))
                        });
                    let now = std::time::SystemTime::now();
                    self.state.mark_read(path, now, mtime);
                }
            }
            t if WRITE_TOOLS.contains(&t) => {
                // notebook_edit 当前不在 ToolOutput.metadata 暴露 path(只
                // 设 `metadata: Value::Null`),所以 forget 仅对 write/edit
                // 生效。notebook cell 级编辑的 forget 暂作 follow-up(v1.1:
                // 让 NotebookEditTool emit `metadata["path"]` + `mtime_ms`,
                // 与 read 工具契约对齐)。
                if matches!(t, "write" | "edit") {
                    if let Some(path_str) = result.metadata.get("path").and_then(|v| v.as_str()) {
                        let path = PathBuf::from(path_str);
                        self.state.forget(&path);
                    }
                }
            }
            _ => {}
        }
        HookDecision::Allow
    }
}

/// 构造 `(SharedFileReadState, ReadBeforeEditHook)` 的一站式工厂。
///
/// `reflect-exec::bootstrap_m4` 与 `reflect-tui::build_app` 都需要:新建
/// 一个 `Arc<FileReadStateTracker>`、把 `enabled` / `drift` 透传给
/// `ReadBeforeEditConfig`、构造 hook。把这些集中在一处,避免两处
/// 重复实现随 hook 签名变化而漂移(参见 review bug-5)。
///
/// 调用方负责 `register_hook(hook)`,并把返回的 `state` Arc 注入到
/// `ToolContext`(给 `read` tool 共享使用)。
pub fn build_read_before_edit(
    enabled: Option<bool>,
    mtime_drift_tolerance_ms: Option<u64>,
) -> (SharedFileReadState, ReadBeforeEditHook) {
    let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
    let config = ReadBeforeEditConfig {
        enabled,
        mtime_drift_tolerance_ms,
    };
    let hook = ReadBeforeEditHook::new(state.clone(), config);
    (state, hook)
}

#[async_trait]
impl Hook for ReadBeforeEditHook {
    fn name(&self) -> &str {
        "read_before_edit"
    }

    fn description(&self) -> &str {
        "PreToolUse: deny write/edit unless the file was read in this session \
         (defense against accidental blind-writes; bash tool is not intercepted)"
    }

    fn events(&self) -> &[HookEventKind] {
        // 订阅 PreToolUse(拦截) + PostToolUse(标记 / 清理)。
        // 不订阅 PostToolUseFailure —— 失败结果不参与状态机。
        &[HookEventKind::PreToolUse, HookEventKind::PostToolUse]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        match event.kind() {
            HookEventKind::PreToolUse => Ok(self.on_pre(event).await),
            HookEventKind::PostToolUse => Ok(self.on_post(event).await),
            _ => Ok(HookDecision::Allow),
        }
    }
}

/// 辅助:把任意 hook 错误渲染成 `HookDecision::Deny`(fail-closed
/// 安全网,虽然 `HookEngine` 已经做了)。
fn _arc_marker(_: Arc<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookContext;
    use reflect_protocol::{ThreadId, ToolOutput, TurnId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_workspace() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("reflect_rbe_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_with_workspace(ws: &std::path::Path, mode: PermissionMode) -> HookContext {
        HookContext {
            session_id: ThreadId::new(),
            turn_id: TurnId::new(),
            workspace: ws.to_path_buf(),
            permission_mode: mode,
        }
    }

    fn pre_event(
        tool: &str,
        args: serde_json::Value,
        mode: PermissionMode,
        ws: &std::path::Path,
    ) -> HookEvent {
        HookEvent::PreToolUse {
            tool: tool.into(),
            args,
            ctx: ctx_with_workspace(ws, mode),
        }
    }

    fn post_event(tool: &str, result: ToolOutput) -> HookEvent {
        HookEvent::PostToolUse {
            tool: tool.into(),
            result,
            elapsed_ms: 0,
        }
    }

    fn tool_output_ok(metadata: serde_json::Value) -> ToolOutput {
        ToolOutput {
            content: vec![],
            is_error: false,
            metadata,
            elapsed_ms: 0,
        }
    }

    fn tool_output_err() -> ToolOutput {
        ToolOutput {
            content: vec![],
            is_error: true,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        }
    }

    /// 1. PreToolUse `write` 未读 → Deny("never read")。
    #[tokio::test]
    async fn write_on_never_read_is_denied() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Auto,
            &ws,
        );
        match h.handle(&ev).await.unwrap() {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("never read"), "{reason}");
                assert!(reason.contains("a.txt"), "{reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// 2. PreToolUse `edit` 未读 → Deny。
    #[tokio::test]
    async fn edit_on_never_read_is_denied() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("b.txt"), "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = pre_event(
            "edit",
            serde_json::json!({"path": "b.txt", "old_string": "x", "new_string": "y"}),
            PermissionMode::Auto,
            &ws,
        );
        match h.handle(&ev).await.unwrap() {
            HookDecision::Deny { reason } => assert!(reason.contains("never read")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// 3. read 成功 → tracker 标记 → 后续 write Allow。
    #[tokio::test]
    async fn read_then_write_allows() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state.clone());

        // 模拟 read tool 成功返回(用 canonical path + mtime_ms)
        let canonical =
            FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let post = post_event(
            "read",
            tool_output_ok(serde_json::json!({
                "path": canonical.to_string_lossy(),
                "mtime_ms": mtime_ms,
            })),
        );
        assert_eq!(h.handle(&post).await.unwrap(), HookDecision::Allow);

        // 后续 write Allow
        let pre = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "new"}),
            PermissionMode::Auto,
            &ws,
        );
        assert_eq!(h.handle(&pre).await.unwrap(), HookDecision::Allow);
    }

    /// 4. PreToolUse write 触发 mtime 漂移 → Deny("modified")。
    #[tokio::test]
    async fn write_after_drift_is_denied() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state.clone());

        let canonical =
            FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        h.handle(&post_event(
            "read",
            tool_output_ok(serde_json::json!({
                "path": canonical.to_string_lossy(),
                "mtime_ms": mtime_ms,
            })),
        ))
        .await
        .unwrap();

        // 把 mtime 推到 +2s,远超 500ms 容差。
        let future = mtime + std::time::Duration::from_secs(2);
        filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();

        let pre = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Auto,
            &ws,
        );
        match h.handle(&pre).await.unwrap() {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("modified"), "{reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// 5. PreToolUse write 在容差内 → Allow。
    #[tokio::test]
    async fn write_within_tolerance_allows() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state.clone());

        let canonical =
            FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        h.handle(&post_event(
            "read",
            tool_output_ok(serde_json::json!({
                "path": canonical.to_string_lossy(),
                "mtime_ms": mtime_ms,
            })),
        ))
        .await
        .unwrap();

        // +100ms 漂移,容差 500ms → Allow。
        let future = mtime + std::time::Duration::from_millis(100);
        filetime::set_file_mtime(&canonical, filetime::FileTime::from_system_time(future)).unwrap();

        let pre = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Auto,
            &ws,
        );
        assert_eq!(h.handle(&pre).await.unwrap(), HookDecision::Allow);
    }

    /// 6. PreToolUse any tool 在 Plan mode → Allow(PlanModeGate 优先)。
    #[tokio::test]
    async fn plan_mode_bypasses_gate() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Plan,
            &ws,
        );
        // hook 早退 Allow(由 PlanModeGate 提供 Deny)。
        assert_eq!(h.handle(&ev).await.unwrap(), HookDecision::Allow);
    }

    /// 7. PreToolUse 非 write/edit 工具(bash/grep/read)→ Allow。
    #[tokio::test]
    async fn non_intercept_tools_are_allowed() {
        let ws = tmp_workspace();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        for tool in ["bash", "grep", "glob", "read", "echo"] {
            let ev = pre_event(
                tool,
                serde_json::json!({"path": "a.txt"}),
                PermissionMode::Auto,
                &ws,
            );
            assert_eq!(
                h.handle(&ev).await.unwrap(),
                HookDecision::Allow,
                "{tool} 应允许"
            );
        }
    }

    /// 8. PostToolUse read 失败(is_error=true)→ tracker 不标记。
    #[tokio::test]
    async fn read_error_does_not_mark() {
        // workspace 不参与本测试,只是给 hook 一个真实 Arc 句柄。
        let _ws = tmp_workspace();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state.clone());

        let post = post_event("read", tool_output_err());
        h.handle(&post).await.unwrap();

        // tracker 仍空 —— 后续 write 仍 Deny。
        assert_eq!(state.len(), 0);
    }

    /// 9. write 成功 → forget → 后续 write 需重 read。
    #[tokio::test]
    async fn write_success_forgets_entry() {
        let ws = tmp_workspace();
        let file = ws.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state.clone());

        // 先 read 标记
        let canonical =
            FileReadStateTracker::canonicalize(&ws, std::path::Path::new("a.txt")).unwrap();
        let mtime = std::fs::metadata(&canonical).unwrap().modified().unwrap();
        let mtime_ms = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        h.handle(&post_event(
            "read",
            tool_output_ok(serde_json::json!({
                "path": canonical.to_string_lossy(),
                "mtime_ms": mtime_ms,
            })),
        ))
        .await
        .unwrap();
        assert_eq!(state.len(), 1);

        // write 成功 → forget
        h.handle(&post_event(
            "write",
            tool_output_ok(serde_json::json!({
                "path": canonical.to_string_lossy(),
            })),
        ))
        .await
        .unwrap();
        assert_eq!(state.len(), 0);

        // 后续 write 不允许(需要重 read)
        let pre = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "z"}),
            PermissionMode::Auto,
            &ws,
        );
        match h.handle(&pre).await.unwrap() {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("never read"), "{reason}");
            }
            other => panic!("expected Deny after forget, got {other:?}"),
        }
    }

    /// 10. enabled=false → PreToolUse 永远 Allow。
    #[tokio::test]
    async fn disabled_config_always_allows() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let cfg = ReadBeforeEditConfig {
            enabled: Some(false),
            mtime_drift_tolerance_ms: Some(500),
        };
        let h = ReadBeforeEditHook::new(state, cfg);
        let ev = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Auto,
            &ws,
        );
        assert_eq!(h.handle(&ev).await.unwrap(), HookDecision::Allow);
    }

    /// 元数据:name/description/events 稳定。
    #[test]
    fn metadata_is_stable() {
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        assert_eq!(h.name(), "read_before_edit");
        assert!(
            h.description().contains("deny write/edit"),
            "description should summarize behavior: {}",
            h.description()
        );
        assert_eq!(
            h.events(),
            &[HookEventKind::PreToolUse, HookEventKind::PostToolUse]
        );
    }

    /// `enabled()` / `drift_tolerance_ms()` 默认值正确。
    #[test]
    fn config_defaults_are_sane() {
        let cfg = ReadBeforeEditConfig::default();
        assert!(cfg.enabled());
        assert_eq!(cfg.drift_tolerance_ms(), 500);
        let none_cfg = ReadBeforeEditConfig {
            enabled: None,
            mtime_drift_tolerance_ms: None,
        };
        assert!(none_cfg.enabled());
        assert_eq!(none_cfg.drift_tolerance_ms(), 500);
    }

    /// Stop / SessionStart 等事件不在 hook 范围 —— Allow。
    #[tokio::test]
    async fn ignores_other_events() {
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = HookEvent::SessionStart {
            session_id: ThreadId::new(),
            config: serde_json::json!({}),
        };
        assert_eq!(h.handle(&ev).await.unwrap(), HookDecision::Allow);
    }

    /// 缺 `path` 字段 → Allow(让工具层 InvalidArgs 拒绝,避免重复错误)。
    #[tokio::test]
    async fn missing_path_arg_lets_tool_reject() {
        let ws = tmp_workspace();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = pre_event(
            "write",
            serde_json::json!({"content": "x"}), // no path
            PermissionMode::Auto,
            &ws,
        );
        assert_eq!(h.handle(&ev).await.unwrap(), HookDecision::Allow);
    }

    /// workspace 不存在(PreToolUse 时)→ Deny with workspace error。
    #[tokio::test]
    async fn workspace_unreachable_denies() {
        let _ws = PathBuf::from("/definitely/does/not/exist/anywhere");
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        let ev = pre_event(
            "write",
            serde_json::json!({"path": "a.txt", "content": "y"}),
            PermissionMode::Auto,
            std::path::Path::new("/definitely/does/not/exist/anywhere"),
        );
        match h.handle(&ev).await.unwrap() {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("cannot resolve"), "{reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── notebook_edit 子动作区分 ────────────────────────────────────

    /// `notebook_edit` 的 list/read 子动作天然只读,无需 prior read。
    #[tokio::test]
    async fn notebook_edit_readonly_actions_bypass_gate() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("nb.ipynb"), r#"{"cells":[]}"#).unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        for (action, args) in [
            (
                "list",
                serde_json::json!({"action": "list", "path": "nb.ipynb"}),
            ),
            (
                "read",
                serde_json::json!({"action": "read", "path": "nb.ipynb", "index": 0}),
            ),
        ] {
            let ev = pre_event("notebook_edit", args, PermissionMode::Auto, &ws);
            assert_eq!(
                h.handle(&ev).await.unwrap(),
                HookDecision::Allow,
                "notebook_edit {action} 应允许(只读子动作)"
            );
        }
    }

    /// `notebook_edit` 的 edit/insert/delete 子动作会写盘,要求 prior read。
    #[tokio::test]
    async fn notebook_edit_write_actions_require_read() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("nb.ipynb"), r#"{"cells":[]}"#).unwrap();
        let state: SharedFileReadState = Arc::new(FileReadStateTracker::new());
        let h = ReadBeforeEditHook::with_defaults(state);
        for (action, args) in [
            (
                "edit",
                serde_json::json!({
                    "action": "edit", "path": "nb.ipynb",
                    "index": 0, "source": "x"
                }),
            ),
            (
                "insert",
                serde_json::json!({
                    "action": "insert", "path": "nb.ipynb",
                    "index": 0, "source": "x"
                }),
            ),
            (
                "delete",
                serde_json::json!({
                    "action": "delete", "path": "nb.ipynb", "index": 0
                }),
            ),
        ] {
            let ev = pre_event("notebook_edit", args, PermissionMode::Auto, &ws);
            match h.handle(&ev).await.unwrap() {
                HookDecision::Deny { reason } => {
                    assert!(
                        reason.contains("never read"),
                        "notebook_edit {action} 未 read 应 Deny 含 'never read',实际: {reason}"
                    );
                }
                other => panic!("notebook_edit {action} 未 read 应 Deny, got {other:?}"),
            }
        }
    }

    /// `WRITE_TOOLS` 白名单 + `is_write_intent` 分类契约:新增写工具
    /// 时漏掉这里会立即被这个测试捕捉。
    #[test]
    fn write_tools_whitelist_and_intent_classification() {
        // 白名单覆盖三类写工具
        assert!(WRITE_TOOLS.contains(&"write"));
        assert!(WRITE_TOOLS.contains(&"edit"));
        assert!(WRITE_TOOLS.contains(&"notebook_edit"));
        // write / edit: 任意 args 都是写意图(只要有 path)
        assert!(ReadBeforeEditHook::is_write_intent(
            "write",
            &serde_json::json!({"path": "a.txt"})
        ));
        assert!(ReadBeforeEditHook::is_write_intent(
            "edit",
            &serde_json::json!({"path": "a.txt"})
        ));
        // notebook_edit: 只在 edit/insert/delete 子动作时拦截
        assert!(ReadBeforeEditHook::is_write_intent(
            "notebook_edit",
            &serde_json::json!({"action": "edit", "path": "nb.ipynb"})
        ));
        assert!(!ReadBeforeEditHook::is_write_intent(
            "notebook_edit",
            &serde_json::json!({"action": "list", "path": "nb.ipynb"})
        ));
        assert!(!ReadBeforeEditHook::is_write_intent(
            "notebook_edit",
            &serde_json::json!({"action": "read", "path": "nb.ipynb", "index": 0})
        ));
        // 未在白名单的工具 → false
        assert!(!ReadBeforeEditHook::is_write_intent(
            "bash",
            &serde_json::json!({"path": "a.txt"})
        ));
        assert!(!ReadBeforeEditHook::is_write_intent(
            "grep",
            &serde_json::json!({"path": "a.txt"})
        ));
    }
}
