//! `checkpoint_tool` — v1.2 P0-3 git 快照(checkpoint)与回退(rewind)工具。
//!
//! 镜像 gap doc P0-3 验证标准:「执行文件修改后 rewind 应恢复到修改前状态」。
//!
//! - `CheckpointTool`:action ∈ {create, list}。`create` 跑
//!   `git_auto_commit` 拍快照 + 写 `RolloutRecord::Checkpoint`;
//!   `list` 读当前 session JSONL 的所有 checkpoint。
//! - `RewindTool`:action = rewind,`scope` 三档:
//!   `workspace`(默认,`git_reset_hard` 到目标 sha,只动文件)、
//!   `conversation`(把 rollout 截断到 checkpoint 所在 turn 之前 ——
//!   经 `reflect_rollout::index::truncate_session_after_turn`,备份 +
//!   原子重写;引擎每轮从 rollout 回放重建历史,截断后下一 turn 自然
//!   从更短历史回放)、`both`(两者同时)。**High 风险**,走 Prompt
//!   审批 + `gate.ask_tool`(镜像 AstTool 的 Deny→is_error 路径)。
//!
//! 取舍:v1.2 只回退工作区文件(会话 JSONL append-only);v1.5 起补齐
//! `conversation` 作用域 —— 截断以 checkpoint 记录的 `turn_id` 为边界,
//! 截断点之后的记录进入 `.bak` 备份,rewind marker 紧跟截断点追加
//! (append-only 语义不破坏)。

use async_trait::async_trait;
use reflect_protocol::{PermissionMode, ToolError, ToolOutput};
use reflect_tools::checkpoint::{
    git_auto_commit, git_current_sha, git_reset_hard, is_valid_commit,
};
use reflect_tools::{Tool, ToolContext};
use serde_json::Value;

/// 工作区 git 快照工具。create / list 两个 action。
pub struct CheckpointTool;

/// 工作区回退工具。rewind action(High 风险,走审批)。
pub struct RewindTool;

#[async_trait]
impl Tool for CheckpointTool {
    fn name(&self) -> &str {
        "checkpoint"
    }

    fn description(&self) -> &str {
        "对 workspace 的 git tree 打快照(后续可用 `rewind` 还原)。\
         操作:`create`(可选 label,返回 sha + checkpoint id)/ `list`(本会话已记录的所有 checkpoint)。\
         仅快照工作区文件 —— 对话历史不受影响。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list"],
                    "description": "Which checkpoint operation to perform."
                },
                "label": {
                    "type": "string",
                    "description": "Optional human-readable label for the checkpoint (create)."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false // create 跑 git commit(有副作用)
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    fn action_permission(&self, args: &Value) -> PermissionMode {
        // create 改工作区(commit);list 只读。
        match args.get("action").and_then(|v| v.as_str()) {
            Some("create") => PermissionMode::Prompt,
            _ => PermissionMode::Auto,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let action =
            args.get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "checkpoint: missing required field 'action'".into(),
                })?;
        match action {
            "create" => self.exec_create(&ctx, &args).await,
            "list" => self.exec_list(&ctx).await,
            other => Err(ToolError::InvalidArgs {
                message: format!("checkpoint: unknown action '{other}'"),
            }),
        }
    }
}

impl CheckpointTool {
    async fn exec_create(&self, ctx: &ToolContext, args: &Value) -> Result<ToolOutput, ToolError> {
        let workspace = ctx.workspace_path();
        let label = args.get("label").and_then(|v| v.as_str());
        let msg = match label {
            Some(l) => format!("reflect checkpoint: {l}"),
            None => "reflect checkpoint".to_string(),
        };
        let sha = git_auto_commit(&workspace, &msg)?;
        // 写 RolloutRecord::Checkpoint(若 recorder / session 可用)。
        // best-effort:写失败只 warn,不阻塞(checkpoint 本身已成功)。
        if let Err(e) = write_checkpoint_record(&ctx, &sha, label) {
            tracing::warn!(error = %e, "checkpoint: failed to persist rollout record (git snapshot still taken)");
        }
        let text = format!(
            "Checkpoint created: `{sha}`{}\nThe workspace git tree is snapshotted; use `rewind` \
             with this sha to restore it later.",
            label.map(|l| format!(" ({l})")).unwrap_or_default()
        );
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text { text }],
            is_error: false,
            metadata: serde_json::json!({
                "action": "create",
                "sha": sha,
                "checkpoint_id": sha,
                "label": label,
            }),
            elapsed_ms: 0,
        })
    }

    async fn exec_list(&self, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let checkpoints = list_checkpoint_records(ctx);
        let summaries: Vec<Value> = checkpoints
            .iter()
            .filter_map(|r| match r {
                reflect_protocol::RolloutRecord::Checkpoint {
                    sha,
                    label,
                    created_at,
                    ..
                } => Some(serde_json::json!({
                    "sha": sha,
                    "checkpoint_id": sha,
                    "label": label,
                    "created_at": created_at,
                })),
                _ => None,
            })
            .collect();
        let text = if summaries.is_empty() {
            "No checkpoints recorded this session. Use `checkpoint create` to snapshot the \
             workspace."
                .to_string()
        } else {
            let mut lines = vec!["Checkpoints this session:".to_string()];
            for s in &summaries {
                let sha = s["sha"].as_str().unwrap_or("?");
                let label = s["label"]
                    .as_str()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default();
                lines.push(format!("  - {sha}{label}"));
            }
            lines.join("\n")
        };
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text { text }],
            is_error: false,
            metadata: serde_json::json!({
                "action": "list",
                "count": summaries.len(),
                "checkpoints": summaries,
            }),
            elapsed_ms: 0,
        })
    }
}

#[async_trait]
impl Tool for RewindTool {
    fn name(&self) -> &str {
        "rewind"
    }

    fn description(&self) -> &str {
        "回退到指定 checkpoint。`scope` 三档:`workspace`(默认,仅把文件树 \
         git reset 到 sha,丢弃未提交变更)、`conversation`(仅把对话历史 \
         截断到 checkpoint 所在 turn 之前,文件不动)、`both`(两者同时)。\
         `sha` 来自 `checkpoint create/list`,省略时自动定位最近的 checkpoint。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sha": {
                    "type": "string",
                    "description": "Target checkpoint sha to restore (from checkpoint create/list). Omit to auto-locate the most recent checkpoint."
                },
                "scope": {
                    "type": "string",
                    "enum": ["workspace", "conversation", "both"],
                    "description": "workspace = files only (default); conversation = truncate conversation history only; both = both."
                }
            },
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // High 风险:丢弃未提交变更 / 截断对话。整体走 Prompt。
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let workspace = ctx.workspace_path();
        // 作用域解析:默认 workspace(向后兼容旧调用 —— 只回滚文件)。
        let rewind_files = matches!(
            args.get("scope").and_then(|v| v.as_str()),
            None | Some("workspace") | Some("both")
        );
        let rewind_conversation = matches!(
            args.get("scope").and_then(|v| v.as_str()),
            Some("conversation") | Some("both")
        );
        if !rewind_files && !rewind_conversation {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "rewind: unknown scope '{}' (expected workspace | conversation | both)",
                    args.get("scope").and_then(|v| v.as_str()).unwrap_or("")
                ),
            });
        }

        // v1.x:sha 缺失时 fallback 到 `find_checkpoint_for_turn` —— 回退到当前
        // turn 对应的最近 checkpoint。此前 `find_checkpoint_for_turn` 是孤儿
        // (实现完整但零调用者),LLM 丢失/记错 sha 时 rewind 直接失败。现在
        // LLM 可以省略 sha 走自动定位,或传 sha 走精确匹配。
        let (sha, checkpoint_turn) = match args.get("sha").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => (s.to_string(), find_checkpoint_turn_by_sha(&ctx, s)),
            _ => {
                // 无 sha → 按当前 turn_id 查最近 checkpoint。
                let base = reflect_rollout::path::default_base();
                match reflect_rollout::index::find_checkpoint_for_turn(
                    &base,
                    ctx.session_id,
                    &ctx.turn_id,
                ) {
                    Some(found) => {
                        let turn = find_checkpoint_turn_by_sha(&ctx, &found);
                        (found, turn)
                    }
                    None => {
                        return Err(ToolError::InvalidArgs {
                            message: "rewind: no 'sha' provided and no checkpoint found for current turn; run `checkpoint create` first or pass an explicit sha".into(),
                        });
                    }
                }
            }
        };

        // conversation / both 需要 checkpoint 记录里的 turn_id 作为截断
        // 边界;sha 对应的 checkpoint 记录不在当前 session 文件中时无法
        // 定位,显式报错(而不是静默只回滚文件)。
        if rewind_conversation && checkpoint_turn.is_none() {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "rewind: scope '{scope}' requires the checkpoint record for sha `{sha}` in the \
                     current session (to locate the conversation boundary), but none was found",
                    scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
                ),
            });
        }

        // 文件回退目标校验(仅 scope 涉及文件时)。
        if rewind_files && !is_valid_commit(&workspace, &sha) {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "rewind: '{sha}' is not a valid commit in {}",
                    workspace.display()
                ),
            });
        }

        // 审批:rewind 丢弃未提交变更 / 截断对话,在 execute 前显式 ask_tool
        // (镜像 AstTool 的 mutation 路径)。Deny → is_error 返回,不抛 Err。
        if let Some(gate) = ctx.approval.as_ref() {
            let approved = gate
                .ask_tool(
                    "rewind",
                    &args,
                    reflect_protocol::RiskLevel::High,
                    &ctx.cancel,
                )
                .await;
            if !matches!(approved, reflect_protocol::ReviewDecision::Approve) {
                return Ok(ToolOutput {
                    content: vec![reflect_protocol::ContentBlock::Text {
                        text: format!("Rewind to `{sha}` was not approved; nothing changed."),
                    }],
                    is_error: true,
                    metadata: serde_json::json!({
                        "action": "rewind",
                        "sha": sha,
                        "approved": false,
                    }),
                    elapsed_ms: 0,
                });
            }
        }

        // ── 文件回退(workspace / both)─────────────────────────────────
        let from_sha = git_current_sha(&workspace).unwrap_or_else(|_| "unknown".into());
        if rewind_files {
            git_reset_hard(&workspace, &sha)?;
        }

        // ── 对话回退(conversation / both)──────────────────────────────
        // 截断 checkpoint 所在 turn(含)之后的 rollout:引擎每轮从 rollout
        // 回放重建历史,截断后下一个 turn 自然从更短的历史回放。
        // 失败不致命:文件已回退(或未动),warn 后继续,输出中注明。
        let mut dropped_messages = 0usize;
        let mut conversation_note = String::new();
        if rewind_conversation {
            let target = checkpoint_turn.expect("checked above");
            match truncate_conversation(&ctx, &target) {
                Ok(dropped) => {
                    dropped_messages = dropped;
                    conversation_note = format!(
                        " Conversation truncated: {dropped} message record(s) after the \
                         checkpoint turn were removed (a .bak backup was kept)."
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "rewind: conversation truncate failed");
                    conversation_note =
                        format!(" Conversation truncation failed: {e:#} (history unchanged).");
                }
            }
        }

        // best-effort 写 Rewind 记录 —— 注意 conversation / both 作用域下
        // 此记录**紧跟截断点之后**追加,不会复活被丢弃的历史,且让下一轮
        // 回放看到「曾在此处回退」的审计轨迹。
        if let Err(e) = write_rewind_record(&ctx, &sha, &from_sha) {
            tracing::warn!(error = %e, "rewind: failed to persist rollout record (rewind itself still done)");
        }
        let files_part = if rewind_files {
            format!(
                "Workspace rewound from `{from_sha}` to `{sha}`. Uncommitted changes and \
                 untracked files since then were discarded."
            )
        } else {
            "Workspace files left untouched (scope=conversation).".to_string()
        };
        let text = format!("{files_part}{conversation_note}");
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text { text }],
            is_error: false,
            metadata: serde_json::json!({
                "action": "rewind",
                "scope": args.get("scope").and_then(|v| v.as_str()).unwrap_or("workspace"),
                "target_sha": sha,
                "from_sha": from_sha,
                "dropped_messages": dropped_messages,
                "approved": true,
            }),
            elapsed_ms: 0,
        })
    }
}

/// 在 session JSONL 里按 sha 反查 checkpoint 记录的 `turn_id`
/// (conversation 作用域的截断边界)。
fn find_checkpoint_turn_by_sha(ctx: &ToolContext, sha: &str) -> Option<reflect_protocol::TurnId> {
    list_checkpoint_records(ctx)
        .into_iter()
        .find_map(|r| match r {
            reflect_protocol::RolloutRecord::Checkpoint {
                turn_id, sha: s, ..
            } if s == sha => Some(turn_id),
            _ => None,
        })
}

/// conversation 作用域的 rollout 截断。复用 `rollout_ctx` 的 base/session
/// 定位(与 checkpoint/rewind 记录写入同源),委托
/// `reflect_rollout::index::truncate_session_after_turn`(备份 + 原子重写)。
fn truncate_conversation(
    ctx: &ToolContext,
    to_turn: &reflect_protocol::TurnId,
) -> Result<usize, String> {
    let (base, sid) = rollout_ctx(ctx).ok_or("rollout context unavailable")?;
    reflect_rollout::index::truncate_session_after_turn(&base, sid, Some(to_turn))
        .map_err(|e| e.to_string())
}

// ── rollout record 写入(best-effort,无 recorder / session 时跳过)────

/// 解析 session JSONL 的 base 目录 + session_id。reflect-exec 启动时把
/// `rollout base` 与 `session_id` 通过环境变量注入(`REFLECT_ROLLOUT_BASE` /
/// `REFLECT_SESSION_ID`),工具据此定位文件;缺失则 best-effort 跳过。
fn rollout_ctx(ctx: &ToolContext) -> Option<(std::path::PathBuf, reflect_protocol::ThreadId)> {
    let base = std::env::var("REFLECT_ROLLOUT_BASE")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME").ok().map(|h| {
                std::path::PathBuf::from(h)
                    .join(".reflect")
                    .join("sessions")
            })
        })?;
    // session_id:优先环境变量,否则用 ToolContext 的 session_id。
    let sid = std::env::var("REFLECT_SESSION_ID")
        .ok()
        .and_then(|s| uuid::Uuid::parse_str(&s).ok())
        .map(reflect_protocol::ThreadId)
        .unwrap_or_else(|| ctx.session_id);
    Some((base, sid))
}

fn write_checkpoint_record(
    ctx: &ToolContext,
    sha: &str,
    label: Option<&str>,
) -> Result<(), String> {
    let (base, sid) = rollout_ctx(ctx).ok_or("rollout context unavailable")?;
    reflect_rollout::index::write_checkpoint_record(&base, sid, ctx.turn_id, sha, label)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn write_rewind_record(ctx: &ToolContext, target_sha: &str, from_sha: &str) -> Result<(), String> {
    let (base, sid) = rollout_ctx(ctx).ok_or("rollout context unavailable")?;
    reflect_rollout::index::write_rewind_record(&base, sid, ctx.turn_id, target_sha, from_sha)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn list_checkpoint_records(ctx: &ToolContext) -> Vec<reflect_protocol::RolloutRecord> {
    let Some((base, sid)) = rollout_ctx(ctx) else {
        return Vec::new();
    };
    reflect_rollout::index::list_checkpoints(&base, sid).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::{ReviewDecision, RolloutRecorder as _, ThreadId};
    use reflect_tools::ToolContext;

    /// 改 `REFLECT_ROLLOUT_BASE` env 的用例必须持有的进程级锁(env 是
    /// 进程全局,并行用例共享)。
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[allow(clippy::field_reassign_with_default)] // workspace 须经 set_workspace method,无法在 literal 内设置
    fn tmp_repo_ctx() -> (ToolContext, std::path::PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("reflect_ckpttool_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        reflect_tools::worktree::run_git(&dir, &["init"]).unwrap();
        reflect_tools::worktree::run_git(&dir, &["config", "user.email", "t@t.t"]).unwrap();
        reflect_tools::worktree::run_git(&dir, &["config", "user.name", "T"]).unwrap();
        let mut ctx = ToolContext::default();
        ctx.session_id = ThreadId::new();
        ctx.set_workspace(dir.clone());
        (ctx, dir)
    }

    #[tokio::test]
    async fn checkpoint_create_returns_sha_and_commits() {
        let (ctx, dir) = tmp_repo_ctx();
        std::fs::write(dir.join("a.txt"), "v1").unwrap();
        let out = CheckpointTool
            .execute(ctx, serde_json::json!({"action":"create","label":"first"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        let sha = out.metadata["sha"].as_str().unwrap();
        assert_eq!(sha.len(), 40);
        assert_eq!(out.metadata["label"], "first");
    }

    #[tokio::test]
    async fn checkpoint_list_empty_when_none() {
        let (ctx, _dir) = tmp_repo_ctx();
        let out = CheckpointTool
            .execute(ctx, serde_json::json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["count"], 0);
    }

    #[tokio::test]
    async fn rewind_restores_files_after_modification() {
        // gap doc 核心验证用例:checkpoint → 改 → rewind → 恢复。
        let (ctx, dir) = tmp_repo_ctx();
        std::fs::write(dir.join("a.txt"), "original").unwrap();
        let cp = CheckpointTool
            .execute(ctx.clone(), serde_json::json!({"action":"create"}))
            .await
            .unwrap();
        let sha = cp.metadata["sha"].as_str().unwrap().to_string();

        // 修改文件。
        std::fs::write(dir.join("a.txt"), "modified").unwrap();
        std::fs::write(dir.join("b.txt"), "untracked").unwrap();

        // rewind(无 approval gate → 直接执行)。
        let out = RewindTool
            .execute(ctx, serde_json::json!({"sha": sha}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["approved"], true);
        // 文件恢复。
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "original"
        );
        assert!(!dir.join("b.txt").exists());
    }

    #[tokio::test]
    async fn rewind_invalid_sha_errors() {
        let (ctx, _dir) = tmp_repo_ctx();
        let err = RewindTool
            .execute(ctx, serde_json::json!({"sha":"not-a-real-sha"}))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn checkpoint_action_permission_routes_create_to_prompt() {
        assert_eq!(
            CheckpointTool.action_permission(&serde_json::json!({"action":"create"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            CheckpointTool.action_permission(&serde_json::json!({"action":"list"})),
            PermissionMode::Auto
        );
    }

    #[tokio::test]
    async fn rewind_required_permission_is_prompt() {
        assert_eq!(RewindTool.required_permission(), PermissionMode::Prompt);
    }

    /// 无 approval gate(headless / 测试)时 rewind 直接执行(由
    /// `required_permission = Prompt` + queue 在 TUI 注入 gate;此处 ctx
    /// 无 gate,execute 走 no-gate 分支)。Deny 路径需 ApprovalGate 端到端
    /// 驱动,见 reflect-tui approval 测试套件,此处不重复。
    #[tokio::test]
    async fn rewind_runs_without_gate() {
        let (ctx, dir) = tmp_repo_ctx();
        std::fs::write(dir.join("a.txt"), "v1").unwrap();
        let cp = CheckpointTool
            .execute(ctx.clone(), serde_json::json!({"action":"create"}))
            .await
            .unwrap();
        let sha = cp.metadata["sha"].as_str().unwrap().to_string();
        let out = RewindTool
            .execute(ctx, serde_json::json!({"sha": sha}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // ReviewDecision 引入仅用于文档化 deny 变体名,避免 unused import。
        let _ = ReviewDecision::Approve;
    }

    /// conversation 作用域端到端:checkpoint 之后的历史被截断,文件不动。
    /// 需要 `REFLECT_ROLLOUT_BASE` 指向临时目录(rollout_ctx 读 env),
    /// 以共享的 ENV_LOCK 串行化避免污染同进程并行用例。
    #[tokio::test]
    async fn rewind_conversation_scope_truncates_rollout() {
        let _g = ENV_LOCK.lock().await;
        let (ctx, dir) = tmp_repo_ctx();
        let rollout_base = tempfile::tempdir().unwrap();
        // SAFETY:测试进程内以 ENV_LOCK 串行化,用例结束即恢复。
        unsafe { std::env::set_var("REFLECT_ROLLOUT_BASE", rollout_base.path()) };

        // 造 session 文件:SessionMeta + checkpoint 之前的 1 条历史。
        let w = reflect_rollout::JsonlRolloutWriter::new(rollout_base.path(), ctx.session_id);
        w.record(reflect_protocol::RolloutRecord::session_meta(
            ctx.session_id,
            "mock",
        ))
        .await
        .unwrap();
        let pre_turn = reflect_protocol::TurnId::new();
        w.record(reflect_protocol::RolloutRecord::message(
            pre_turn,
            reflect_protocol::MessageRole::User,
            serde_json::json!("hi"),
        ))
        .await
        .unwrap();

        // checkpoint(git commit v1)→ 之后追加 2 条历史 + 文件修改。
        std::fs::write(dir.join("a.txt"), "v1").unwrap();
        let cp = CheckpointTool
            .execute(ctx.clone(), serde_json::json!({"action":"create"}))
            .await
            .unwrap();
        let sha = cp.metadata["sha"].as_str().unwrap().to_string();
        let post_turn = reflect_protocol::TurnId::new();
        w.record(reflect_protocol::RolloutRecord::message(
            post_turn,
            reflect_protocol::MessageRole::Assistant,
            serde_json::json!("work"),
        ))
        .await
        .unwrap();
        w.record(reflect_protocol::RolloutRecord::message(
            post_turn,
            reflect_protocol::MessageRole::Assistant,
            serde_json::json!("more"),
        ))
        .await
        .unwrap();
        std::fs::write(dir.join("a.txt"), "modified").unwrap();

        // rewind scope=conversation:文件保持 modified,rollout 截断到
        // checkpoint turn 之前(2 条 post 消息 + checkpoint 记录被丢弃)。
        let sid = ctx.session_id;
        let out = RewindTool
            .execute(ctx, serde_json::json!({"scope":"conversation","sha": sha}))
            .await
            .unwrap();
        assert!(!out.is_error, "output: {:?}", out.metadata);
        assert_eq!(out.metadata["dropped_messages"], 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "modified",
            "conversation scope must not touch files"
        );

        // session 文件:SessionMeta + 1 条 pre 消息 + 截断后追加的 Rewind
        // marker;checkpoint 记录与 post 消息均已不在。
        let path = reflect_rollout::index::find_session_path(rollout_base.path(), sid)
            .expect("session file must exist");
        let body = std::fs::read_to_string(&path).unwrap();
        // 注意不能断言 `!body.contains(&sha)`:截断后追加的 Rewind marker
        // 自身携带 target_sha。改断言 checkpoint 类型行已消失。
        assert!(
            !body.contains("\"type\":\"checkpoint\""),
            "checkpoint record must be truncated out: {body}"
        );
        assert!(
            body.contains("\"type\":\"rewind\""),
            "rewind marker kept: {body}"
        );
        let msgs = body
            .lines()
            .filter(|l| l.contains("\"type\":\"message\""))
            .count();
        assert_eq!(msgs, 1, "only the pre-checkpoint message survives: {body}");

        // 恢复 env,避免泄漏到其它并行用例。
        // SAFETY:同上,ENV_LOCK 保护下恢复。
        unsafe { std::env::remove_var("REFLECT_ROLLOUT_BASE") };
    }

    /// both 作用域:文件回滚 + 对话截断同时发生。
    #[tokio::test]
    async fn rewind_both_scope_restores_files_and_truncates_history() {
        let _g = ENV_LOCK.lock().await;
        let (ctx, dir) = tmp_repo_ctx();
        let rollout_base = tempfile::tempdir().unwrap();
        // SAFETY:ENV_LOCK 串行化。
        unsafe { std::env::set_var("REFLECT_ROLLOUT_BASE", rollout_base.path()) };

        let w = reflect_rollout::JsonlRolloutWriter::new(rollout_base.path(), ctx.session_id);
        w.record(reflect_protocol::RolloutRecord::session_meta(
            ctx.session_id,
            "mock",
        ))
        .await
        .unwrap();
        std::fs::write(dir.join("a.txt"), "original").unwrap();
        let cp = CheckpointTool
            .execute(ctx.clone(), serde_json::json!({"action":"create"}))
            .await
            .unwrap();
        let sha = cp.metadata["sha"].as_str().unwrap().to_string();
        let post_turn = reflect_protocol::TurnId::new();
        w.record(reflect_protocol::RolloutRecord::message(
            post_turn,
            reflect_protocol::MessageRole::Assistant,
            serde_json::json!("later"),
        ))
        .await
        .unwrap();
        std::fs::write(dir.join("a.txt"), "modified").unwrap();
        let sid = ctx.session_id;

        let out = RewindTool
            .execute(ctx, serde_json::json!({"scope":"both","sha": sha}))
            .await
            .unwrap();
        assert!(!out.is_error, "output: {:?}", out.metadata);
        assert_eq!(out.metadata["dropped_messages"], 1);
        // 文件恢复。
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "original"
        );
        // 对话截断。
        let path = reflect_rollout::index::find_session_path(rollout_base.path(), sid).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("\"type\":\"checkpoint\""));
        assert!(!body.contains("later"), "post-checkpoint history dropped");

        // SAFETY:ENV_LOCK 保护下恢复。
        unsafe { std::env::remove_var("REFLECT_ROLLOUT_BASE") };
    }

    /// 未知 scope 显式报错。
    #[tokio::test]
    async fn rewind_unknown_scope_errors() {
        let (ctx, _dir) = tmp_repo_ctx();
        let err = RewindTool
            .execute(ctx, serde_json::json!({"scope":"everything"}))
            .await;
        assert!(err.is_err());
    }
}
