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
//! `zhuye-exec` —— headless 二进制。读取 prompt,运行单个回合,
//! 以 JSONL 流形式向 stdout 输出事件。tracing 日志走 stderr。

mod bootstrap;
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
use zhuye_protocol::{EventMsg, Submission, TurnStatus};

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
    /// `zhuye exec --resume <UUID>` 直接报 "unexpected argument" 退出。
    /// 补回 long flag,与 `-c` / `-r` 一致地暴露给 CLI,并用
    /// `conflicts_with_all` 在 clap 层 enforce 三者互斥。
    #[arg(long, value_name = "UUID", conflicts_with_all = ["continue_last", "resume_by"])]
    pub resume: Option<String>,
    /// 续最近一次 session(等价 `--resume $(zhuye session ls | head -1)`)。
    #[arg(long, short = 'c', conflicts_with_all = ["resume", "resume_by"])]
    pub continue_last: bool,
    /// 按序号 resume(`zhuye exec -r 3` = 续第 3 条 session,1-indexed,newest first)。
    #[arg(long, short = 'r', value_name = "N", conflicts_with_all = ["resume", "continue_last"])]
    pub resume_by: Option<usize>,
    /// Agent 定义名(在 `~/.zhuye/agents/*.md` 与
    /// `{workspace}/.zhuye/agents/*.md` 中查找)。默认 `"default"`。
    #[arg(long)]
    pub agent: Option<String>,
    /// v1.x:启动后立即进入 Plan mode(只读调研,write 工具被 `PlanModeGate`
    /// hook blanket-deny)。等价 `/plan` slash,但作用在 `zhuye exec` 的
    /// headless 单 turn 模式:agent 只能跑 read-only 工具;turn 失败时
    /// 进程以 exit code 122 退出(CI / 脚本可据此重试)。
    #[arg(long)]
    pub plan_mode: bool,
    /// S2.5:用内存 `InMemoryTaskStore` 而**不**用 `FileTaskStore` —— 任务
    /// 不会持久化到 `~/.zhuye/tasks/<list>/<id>.json`。默认 `false`
    /// (持久化,与 CLI 的 `zhuye task ls` 同源)。
    ///
    /// 何时启用:单元测试 / 临时跑一次不想留痕 / `HOME` 未设。
    #[arg(long, default_value_t = false)]
    pub ephemeral_tasks: bool,
    /// S2.5:用内存 `InMemoryTeamStore` 而**不**用 `FileTeamStore` —— team
    /// 不会持久化到 `~/.zhuye/teams/<name>.json`。默认 `false`
    /// (持久化,与 CLI 的 `zhuye task team` 同源)。
    #[arg(long, default_value_t = false)]
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根(第一个含 `.git` / `Cargo.toml` / `.zhuye/` 的
    /// 目录)作为工作区。默认关;与 `zhuye tui --auto-root` 对齐。
    #[arg(long, default_value_t = false)]
    pub auto_root: bool,
    /// v1.7:图片附件(可多次传入)。每个文件读为 `UserInputItem::Image`
    /// block,与 prompt 一并提交 —— 打通协议层已支持的多模态输入(此前
    /// 只有 `image_view` 工具能产生 Image block,CLI 无入口)。支持
    /// png/jpeg/gif/webp/bmp,单张 ≤10MiB。与 `--resume`/`-c`/`-r` 互斥。
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<String>,
}

/// `--plan-mode` 下"计划生成失败"的退出码。
///
/// `docs/PLAN_MODE.md:50` 与 `ExecArgs::plan_mode` 的 doc 都承诺了这个
/// 退出码,但**全仓库没有任何实现** —— `async_main` 正常路径恒返回
/// `Ok(())`(exit 0),依赖 122 做重试判断的脚本调用方会永远看到"成功"。
///
/// 判定标准:Plan mode 下 agent 调研完成后调 `ExitPlanMode` 成功,core 会
/// 派发 `EventMsg::PlanReady`(见 `graph/nodes/tool_exec.rs` 的
/// `ExitPlanMode` 分支)。所以"流过了 `PlanReady` / `PlanApproved`"就等价于
/// "计划已产出";turn 结束仍没见过 → 计划生成失败,需要重试。
pub const PLAN_MODE_EXIT_CODE: i32 = 122;

/// 判断一个 event 是否代表"计划已成功产出"。
fn is_plan_ready_event(event: &zhuye_protocol::Event) -> bool {
    matches!(
        event.msg,
        zhuye_protocol::EventMsg::PlanReady(_) | zhuye_protocol::EventMsg::PlanApproved(_)
    )
}

/// `--plan-mode` 收尾:产出过计划 → `Ok(())`;否则按文档约定 exit 122。
///
/// 只在显式请求了 Plan mode 时生效,普通 headless 调用不受影响。
fn finish_plan_mode(plan_requested: bool, plan_produced: bool) -> anyhow::Result<()> {
    if plan_requested && !plan_produced {
        tracing::warn!(
            exit_code = PLAN_MODE_EXIT_CODE,
            "plan_mode: turn 结束但未产出计划(未见 PlanReady),按文档约定退出 122"
        );
        // 直接退出而非返回 Err:Err 会让 main 打印错误链并 exit 1,
        // 与约定的 122 混淆。
        std::process::exit(PLAN_MODE_EXIT_CODE);
    }
    Ok(())
}

/// 入口函数:既被独立 `zhuye-exec` 二进制调用,也被顶层 CLI router 的
/// `zhuye exec` 子命令调用。
///
/// review 修复:此前 turn 失败(Error / 非 Success 的 TurnComplete)进程仍
/// 退出 0,CI / 脚本无法感知失败,`--plan-mode` 帮助里承诺的 exit 122 也从未
/// 实现。现在按终结事件映射退出码(见 [`async_main`]),非零码直接
/// `process::exit`(block_on 已返回,异步侧已收尾)。
pub fn run(args: ExecArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(async_main(args))?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// 终结事件 → 进程退出码。
///
/// - 正常:Success → 0;非 Success(MaxIterations / TokenBudgetExceeded)、
///   Error 事件、TurnAborted → 1。
/// - Plan mode(`--plan-mode`):任何失败 → 122(计划生成失败需要重试)。
fn exit_code_for(args: &ExecArgs, msg: &EventMsg) -> i32 {
    let failed = match msg {
        EventMsg::TurnComplete(tc) => tc.status != TurnStatus::Success,
        EventMsg::Error(_) | EventMsg::TurnAborted(_) => true,
        _ => false,
    };
    if !failed {
        0
    } else if args.plan_mode {
        122
    } else {
        1
    }
}

async fn async_main(args: ExecArgs) -> anyhow::Result<i32> {
    // v1.3 SDK:装配段已抽到 `headless` 模块(与 `zhuye serve` 共享),
    // exec 只保留"resume 三选一 → 提交单条 prompt → 流式输出"的编排。
    // v1.7:图片附件走多模态 UserInput,与 resume 语义互斥(历史回放 +
    // 合成续跑 prompt 的路径不支持附加 item)。
    if !args.images.is_empty()
        && (args.resume.is_some() || args.continue_last || args.resume_by.is_some())
    {
        anyhow::bail!("--image 不能与 --resume/-c/-r 同时使用");
    }
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
        let mut plan_produced = false;
        let mut code = 0;
        while let Some(event) = handle.next().await {
            if is_plan_ready_event(&event) {
                plan_produced = true;
            }
            // 一般失败(Error / 非 Success 的 TurnComplete / TurnAborted)→
            // 非零退出码,CI 与脚本可据此感知失败。
            code = exit_code_for(&args, &event.msg);
            if let Err(e) = writer.write_event(&event) {
                tracing::warn!(error = %e, "jsonl writer failed; exiting");
                break;
            }
        }
        // plan 模式未产出计划 → 按上游约定 exit 122;其余失败由上面的
        // `exit_code_for` 映射成 0 / 1 / 122。
        finish_plan_mode(args.plan_mode, plan_produced)?;
        return Ok(code);
    }

    // ── 普通分支:单条 prompt → JSONL 流 ──
    if args.prompt.is_none() && args.images.is_empty() {
        anyhow::bail!("usage: zhuye exec <prompt>");
    }
    let prompt = args.prompt.clone().unwrap_or_default();
    let session = headless::bootstrap_normal(common, &hargs).await?;
    // v1.7:图片附件 → 多模态 UserInput(items = [Image.., Text?]),不经
    // 插件 slash 展开(命令展开是纯文本路径)。
    let sub = if args.images.is_empty() {
        // v1.6.1:内置 `/prompt` slash 优先于插件展开(引擎级命令)。
        if let Some(prompt_sub) = headless::expand_prompt_slash(&prompt) {
            prompt_sub
        } else {
            // 插件 slash 命令展开:`/demo:hello world` → 命令 md 正文;未命中
            // 任何命令时原样提交。展开失败(命令文件不可读)直接报错退出。
            headless::expand_plugin_command(&session.plugin_runtime, prompt).await?
        }
    } else {
        let mut items = Vec::new();
        for path in &args.images {
            items.push(headless::load_image_item(path)?);
        }
        if !prompt.trim().is_empty() {
            items.push(zhuye_protocol::UserInputItem::Text { text: prompt });
        }
        zhuye_protocol::Submission::user_input_items(items)
    };
    let mut handle = session.thread.submit(sub).await;
    let mut plan_produced = false;
    let mut code = 0;
    while let Some(event) = handle.next().await {
        if is_plan_ready_event(&event) {
            plan_produced = true;
        }
        // 一般失败(Error / 非 Success 的 TurnComplete / TurnAborted)→
        // 非零退出码,CI 与脚本可据此感知失败。
        code = exit_code_for(&args, &event.msg);
        let mut w = JsonlWriter::new(std::io::stdout().lock());
        if let Err(e) = w.write_event(&event) {
            // broken pipe(如 | head 已关闭)—— 优雅退出
            tracing::warn!(error = %e, "jsonl writer failed; exiting");
            break;
        }
    }
    finish_plan_mode(args.plan_mode, plan_produced)?;
    Ok(code)
}
