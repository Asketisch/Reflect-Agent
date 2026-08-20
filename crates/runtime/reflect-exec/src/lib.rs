#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! `reflect-exec` —— headless 二进制。读取 prompt,运行单个回合,
//! 以 JSONL 流形式向 stdout 输出事件。tracing 日志走 stderr。

mod bootstrap;
pub mod bootstrap_plugins;
mod checkpoint_tool;
mod cron_tool;
mod headless;
mod jsonl;
mod reload;
mod runtime_config;
pub mod serve;

#[cfg(test)]
mod tests;

// 重导出,保留历史公共 API 表面积。内部也通过 `crate::` 路径引用这些项。
pub use bootstrap::ResumeBundle;
pub use checkpoint_tool::{CheckpointTool, RewindTool};
pub use cron_tool::CronTool;
pub use jsonl::JsonlWriter;
pub use reload::{diff_sections_for_test, handle_reload, spawn_reload_task};

use clap::Args;
use reflect_protocol::Submission;

use headless::HeadlessArgs;
use runtime_config::apply_coordinator_from_config;

/// `exec` 子命令的参数。
///
/// resume 输入语义上"三选一"(`--resume <uuid>` / `-c` / `-r N`)。历史上用
/// field-level `conflicts_with` 在 clap 层 enforce 互斥,但 `clap_derive` 的
/// `conflicts_with` 不允许 forward-ref,在 `Args` flatten 上下文里会踩坑,故曾
/// 移除互斥改由 `async_main` 的 `if/else if` 优先级链兜底 —— 但这导致
/// `resume_flags` 集成测试里 `-c` + `-r` / `-c` + `--resume` / `-r` + `--resume`
/// 三组冲突用例长期红。
///
/// v1.x 修复:改用 `conflicts_with_all`(显式列出同组其余字段名,避免
/// forward-ref 解析问题),在 clap 层恢复三者互斥。位置参数 `prompt` 与
/// resume 输入的互斥仍由 `async_main` 的优先级链兜底
/// (`--resume` > (`-c` / `-r`) > `prompt`)。
#[derive(Debug, Args)]
pub struct ExecArgs {
    /// 待执行的用户 prompt。若同时给出 `--resume` / `-c` / `-r`(优先级更高
    /// —— 详见 `async_main` 的 resume 分支),则本字段被忽略。
    pub prompt: Option<String>,
    /// 按 thread id 恢复历史 session。把 `RolloutRecord::Message` 与
    /// `Compaction` 记录重放到一个全新的 `AgentThread`,并提交一条合成的
    /// `<system-reminder>resumed session</system-reminder>` 用户消息。
    ///
    /// v1.x 回归修复:此前该字段缺 `#[arg]`,clap 不注册 `--resume` 旗标 ——
    /// `reflect exec --resume <UUID>` 直接报 "unexpected argument" 退出。
    /// 补回 long flag,与 `-c` / `-r` 一致地暴露给 CLI,并用
    /// `conflicts_with_all` 在 clap 层 enforce 三者互斥。
    #[arg(long, value_name = "UUID", conflicts_with_all = ["continue_last", "resume_by"])]
    pub resume: Option<String>,
    /// 续最近一次 session(等价 `--resume $(reflect session ls | head -1)`)。
    #[arg(long, short = 'c', conflicts_with_all = ["resume", "resume_by"])]
    pub continue_last: bool,
    /// 按序号 resume(`reflect exec -r 3` = 续第 3 条 session,1-indexed,newest first)。
    #[arg(long, short = 'r', value_name = "N", conflicts_with_all = ["resume", "continue_last"])]
    pub resume_by: Option<usize>,
    /// Agent 定义名(在 `~/.reflect/agents/*.md` 与
    /// `{workspace}/.reflect/agents/*.md` 中查找)。默认 `"default"`。
    #[arg(long)]
    pub agent: Option<String>,
    /// v1.x:启动后立即进入 Plan mode(只读调研,write 工具被 `PlanModeGate`
    /// hook blanket-deny)。等价 `/plan` slash,但作用在 `reflect exec` 的
    /// headless 单 turn 模式:agent 只能跑 read-only 工具,exit code 122
    /// 表示计划生成失败需要重试。详见 `docs/PLAN_MODE.md`。
    #[arg(long)]
    pub plan_mode: bool,
    /// S2.5:用内存 `InMemoryTaskStore` 而**不**用 `FileTaskStore` —— 任务
    /// 不会持久化到 `~/.reflect/tasks/<list>/<id>.json`。默认 `false`
    /// (持久化,与 CLI 的 `reflect task ls` 同源)。
    ///
    /// 何时启用:单元测试 / 临时跑一次不想留痕 / `HOME` 未设。
    #[arg(long, default_value_t = false)]
    pub ephemeral_tasks: bool,
    /// S2.5:用内存 `InMemoryTeamStore` 而**不**用 `FileTeamStore` —— team
    /// 不会持久化到 `~/.reflect/teams/<name>.json`。默认 `false`
    /// (持久化,与 CLI 的 `reflect task team` 同源)。
    #[arg(long, default_value_t = false)]
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根(第一个含 `.git` / `Cargo.toml` / `.reflect/` 的
    /// 目录)作为工作区。默认关;与 `reflect tui --auto-root` 对齐。
    #[arg(long, default_value_t = false)]
    pub auto_root: bool,
}

/// 入口函数:既被独立 `reflect-exec` 二进制调用,也被顶层 CLI router 的
/// `reflect exec` 子命令调用。
pub fn run(args: ExecArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(args))
}

async fn async_main(args: ExecArgs) -> anyhow::Result<()> {
    // v1.3 SDK:装配段已抽到 `headless` 模块(与 `reflect serve` 共享),
    // exec 只保留"resume 三选一 → 提交单条 prompt → 流式输出"的编排。
    let hargs = HeadlessArgs {
        agent: args.agent.clone(),
        ephemeral_tasks: args.ephemeral_tasks,
        ephemeral_teams: args.ephemeral_teams,
        auto_root: args.auto_root,
        plan_mode: args.plan_mode,
        // exec 保持既有行为:未配置 [hooks] = 全部启用(仅 serve 收紧)。
        hooks_explicit_only: false,
    };
    let common = headless::bootstrap_common(&hargs).await?;
    let resume_thread_id_str = headless::resolve_resume_thread_id(
        args.resume.as_deref(),
        args.continue_last,
        args.resume_by,
    )?;

    if let Some(thread_id_str) = resume_thread_id_str.as_deref() {
        // ── resume 分支:回放历史 + 合成续跑 prompt ──
        let session = headless::bootstrap_resumed(common, &hargs, thread_id_str).await?;
        let prompt = headless::resumed_prompt(thread_id_str, session.prior_messages);
        let sub = Submission::user_input(prompt);
        let mut handle = session.thread.submit(sub).await;
        let stdout = std::io::stdout();
        let mut writer = JsonlWriter::new(stdout.lock());
        while let Some(event) = handle.next().await {
            if let Err(e) = writer.write_event(&event) {
                tracing::warn!(error = %e, "jsonl writer failed; exiting");
                break;
            }
        }
        return Ok(());
    }

    // ── 普通分支:单条 prompt → JSONL 流 ──
    let prompt = args
        .prompt
        .ok_or_else(|| anyhow::anyhow!("usage: reflect exec <prompt>"))?;
    let session = headless::bootstrap_normal(common, &hargs).await?;
    let sub = Submission::user_input(prompt);
    let mut handle = session.thread.submit(sub).await;
    while let Some(event) = handle.next().await {
        let mut w = JsonlWriter::new(std::io::stdout().lock());
        if let Err(e) = w.write_event(&event) {
            // broken pipe(如 | head 已关闭)—— 优雅退出
            tracing::warn!(error = %e, "jsonl writer failed; exiting");
            break;
        }
    }
    Ok(())
}
