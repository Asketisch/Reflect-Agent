//! `reflect serve` —— 常驻 stdio JSONL 会话服务,Python / TS SDK 的协议入口。
//!
//! wire 协议(见 `sdks/PROTOCOL.md`):
//! - **stdin**:每行一个 [`Submission`] JSON(`Op` 的 `"type"` snake_case 标签,
//!   与 exec / TUI 共用的 v0 协议);
//! - **stdout**:每行一个 [`Event`] JSON(turn 事件 + session 事件 + 远程工具
//!   请求,统一由单一 writer task 串行写出;tracing 日志走 stderr 不污染)。
//!
//! serve-local Op(不进 core,stdin 循环就地处理):
//! - `RegisterTools`:注册客户端自定义工具(`RemoteTool`);
//! - `ToolExecutionResponse`:投递远程工具执行回执。
//!
//! 生命周期:一个进程 = 一个常驻 `AgentThread`(多轮共享内存状态);
//! stdin EOF 或 `Op::Shutdown` → core 优雅收尾(emit `ShutdownComplete`)→ 退出。

use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;

use reflect_core::AgentThread;
use reflect_protocol::{EVENT_ID_NONE, ErrorEvent, Event, EventMsg, Op, Submission};
use reflect_tools::remote::{RemoteBridge, RemoteTool};
use reflect_tools::{ToolRegistry, ToolSource};

use crate::headless::{
    HeadlessArgs, bootstrap_common, bootstrap_normal, bootstrap_resumed, resolve_resume_thread_id,
};

/// 单次远程工具执行等待回执的默认上限。
const DEFAULT_REMOTE_TOOL_TIMEOUT_SECS: u64 = 120;
/// serve 退出时等待 writer 排空的上限(在跑 turn 的迟到事件直接丢弃)。
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// `serve` 子命令参数(与 `exec` 对齐,但 prompt 由 stdin Submission 驱动)。
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Agent 定义名(默认 `"default"`)。
    #[arg(long)]
    pub agent: Option<String>,
    /// 任务不持久化(内存 store)。
    #[arg(long, default_value_t = false)]
    pub ephemeral_tasks: bool,
    /// 团队不持久化(内存 store)。
    #[arg(long, default_value_t = false)]
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根作为工作区。
    #[arg(long, default_value_t = false)]
    pub auto_root: bool,
    /// 启动即进入 Plan mode(只读)。
    #[arg(long, default_value_t = false)]
    pub plan_mode: bool,
    /// 按 thread id 恢复历史 session(与 `-c` / `-r` 三选一)。
    #[arg(long, value_name = "UUID", conflicts_with_all = ["continue_last", "resume_by"])]
    pub resume: Option<String>,
    /// 续最近一次 session。
    #[arg(long, short = 'c', conflicts_with_all = ["resume", "resume_by"])]
    pub continue_last: bool,
    /// 按序号 resume(1-indexed,newest first)。
    #[arg(long, short = 'r', value_name = "N", conflicts_with_all = ["resume", "continue_last"])]
    pub resume_by: Option<usize>,
}

impl From<&ServeArgs> for HeadlessArgs {
    fn from(a: &ServeArgs) -> Self {
        Self {
            agent: a.agent.clone(),
            ephemeral_tasks: a.ephemeral_tasks,
            ephemeral_teams: a.ephemeral_teams,
            auto_root: a.auto_root,
            plan_mode: a.plan_mode,
            // serve 是 SDK 嵌入入口:内置 hook 需在 `[hooks].enabled`
            // 显式列出才启用(理由见 HeadlessArgs::hooks_explicit_only)。
            hooks_explicit_only: true,
        }
    }
}

/// 入口:构造 tokio runtime 并阻塞跑 [`serve_main`]。
pub fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_main(args))
}

/// 装配 session(或 resume)后进入 stdin 循环;所有事件写进程 stdout。
async fn serve_main(args: ServeArgs) -> anyhow::Result<()> {
    let hargs = HeadlessArgs::from(&args);
    let common = bootstrap_common(&hargs).await?;
    let resume_id =
        resolve_resume_thread_id(args.resume.as_deref(), args.continue_last, args.resume_by)?;
    let session = match resume_id.as_deref() {
        Some(id) => bootstrap_resumed(common, &hargs, id).await?,
        None => bootstrap_normal(common, &hargs).await?,
    };

    let (sink, sink_rx) = mpsc::channel::<Event>(256);
    let writer = tokio::spawn(async move {
        let mut rx = sink_rx;
        while let Some(ev) = rx.recv().await {
            let mut w = crate::jsonl::JsonlWriter::new(std::io::stdout().lock());
            if w.write_event(&ev).is_err() {
                tracing::warn!(id = %ev.id, "serve: stdout 写事件失败;writer 退出");
                break;
            }
        }
        tracing::debug!("serve: writer task exited");
    });

    let stdin = tokio::io::stdin();
    serve_session(session.thread, session.tools, stdin, sink).await?;

    // 有界等待 writer 排空:在跑 turn 的 reader task 可能还持有 sink
    // clone,超时后直接放弃(迟到事件丢弃,进程即将退出)。
    let _ = tokio::time::timeout(WRITER_DRAIN_TIMEOUT, writer).await;
    Ok(())
}

/// serve 核心循环(IO 参数化,便于集成测试用内存管道驱动)。
///
/// `reader` 产 Submission JSONL 行;所有事件(turn / session / 远程工具
/// 请求)汇入 `sink`。返回即代表会话结束(EOF / Shutdown / 读错误)。
pub async fn serve_session<R>(
    thread: Arc<AgentThread>,
    tools: Arc<ToolRegistry>,
    reader: R,
    sink: mpsc::Sender<Event>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    // 握手事件:serve 启动即主动 emit `session_configured`,SDK 的
    // spawn() 以此判定就绪 —— core 只在首条 user_input 处理时才发
    // (submission_loop 的 `!session_emitted` 分支),若依赖它,客户端
    // "等握手再发首条输入"会死锁。core 的首发在下方两条转发路径里
    // 被跳过,wire 上只出现本条。
    {
        let ev = proactive_session_configured(&thread);
        sink.send(ev).await?;
    }
    // 远程工具桥:请求事件经独立通道产出,由专职 task 转发到 sink。
    let (bridge, mut bridge_rx) = RemoteBridge::new(64);
    let mut session_rx = thread.subscribe_session();
    let mut fwd_handles = Vec::new();
    {
        let sink2 = sink.clone();
        fwd_handles.push(tokio::spawn(async move {
            while let Some(ev) = bridge_rx.recv().await {
                if sink2.send(ev).await.is_err() {
                    break;
                }
            }
        }));
    }
    {
        let sink3 = sink.clone();
        fwd_handles.push(tokio::spawn(async move {
            while let Some(ev) = session_rx.recv().await {
                // ShutdownComplete 由 Shutdown 分支同步排空该 submission
                // 的 handle 转发(保证送达后才退出循环);SessionConfigured
                // 由启动时的主动发射覆盖。此处跳过两者,避免重复。
                if matches!(
                    ev.msg,
                    EventMsg::ShutdownComplete | EventMsg::SessionConfigured(_)
                ) {
                    continue;
                }
                if sink3.send(ev).await.is_err() {
                    break;
                }
            }
        }));
    }

    let remote_timeout = remote_tool_timeout_from_env();
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::info!("serve: stdin EOF,shutting down");
                break;
            }
            Err(e) => return Err(anyhow::anyhow!("serve: read stdin failed: {e}")),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let sub: Submission = match serde_json::from_str(trimmed) {
            Ok(s) => s,
            Err(e) => {
                // 坏行不终止会话:emit Error 事件让客户端知道,继续读下一行。
                let preview: String = trimmed.chars().take(120).collect();
                let _ = sink
                    .send(Event::new(
                        EVENT_ID_NONE,
                        EventMsg::Error(ErrorEvent {
                            code: "invalid_submission".into(),
                            message: format!("serve: submission JSONL 解析失败: {e}"),
                            details: Some(serde_json::json!({ "line": preview })),
                        }),
                    ))
                    .await;
                continue;
            }
        };

        // serve-local Op:进程内处理,不进 core。
        match &sub.op {
            Op::RegisterTools { tools: specs } => {
                for spec in specs {
                    register_remote_tool(&tools, spec.clone(), &bridge, remote_timeout, &sink)
                        .await;
                }
            }
            Op::ToolExecutionResponse { call_id, output } => {
                bridge.complete(call_id, output.clone());
            }
            _ if matches!(sub.op, Op::Shutdown) => {
                // 转发给 core:触发 cancel + emit ShutdownComplete(turn
                // 通道与 session 扇出都会带上),随后退出循环。
                let sub_id = sub.id.clone();
                let mut handle = thread.submit(sub).await;
                while let Some(ev) = handle.next().await {
                    if sink.send(ev).await.is_err() {
                        break;
                    }
                }
                // 收尾标记:客户端迭代器已被 shutdown_complete 终结,
                // 此处仅为保持「每条 serve 提交的 submission 恰好一条
                // submission_closed」不变量(消费方此时已摘除 listener,
                // 该事件被无害丢弃)。
                let _ = sink
                    .send(Event::new(sub_id, EventMsg::SubmissionClosed))
                    .await;
                tracing::info!("serve: shutdown requested");
                break;
            }
            _ => {
                // 普通 Submission:转发 core;turn 事件由后台 reader 汇入
                // sink(允许上一 turn 未结束时提交下一 turn,engine 侧
                // 串行排队)。
                let sub_id = sub.id.clone();
                let mut handle = thread.submit(sub).await;
                let sink4 = sink.clone();
                tokio::spawn(async move {
                    while let Some(ev) = handle.next().await {
                        // SessionConfigured 同时走 session 扇出通道(见
                        // submission_loop),由 session 转发 task 统一写出;
                        // 此处跳过避免重复。
                        if matches!(ev.msg, EventMsg::SessionConfigured(_)) {
                            continue;
                        }
                        if sink4.send(ev).await.is_err() {
                            break;
                        }
                    }
                    // v1.3 SDK:per-turn 通道排空 = 该 submission 在 core
                    // 处理完毕,不会再有任何事件 → 发收尾标记。非 turn 操作
                    // (compact / rewind / 权限模式切换 / goal 等)没有
                    // turn_complete 之类的终态事件,SDK 的 `submit_op`
                    // 迭代器靠本事件收尾,否则会永久阻塞;turn 类 submission
                    // 的迭代器已先被 turn_complete/turn_aborted 终结,
                    // listener 已摘除,本事件被无害丢弃。
                    let _ = sink4
                        .send(Event::new(sub_id, EventMsg::SubmissionClosed))
                        .await;
                });
            }
        }
    }

    // 收尾:停长驻转发 task(它们的 sink clone 会让 writer 永远等不到
    // 通道关闭);serve_main 侧的 writer 有界排空兜底。
    for h in fwd_handles {
        h.abort();
    }
    drop(sink);
    Ok(())
}

/// 注册单个远程工具:空名 / 与既有工具撞名时 emit Error 事件并跳过
/// (不覆盖内置或已注册工具 —— 客户端据此换名重试)。
async fn register_remote_tool(
    tools: &Arc<ToolRegistry>,
    spec: reflect_protocol::RemoteToolSpec,
    bridge: &Arc<RemoteBridge>,
    timeout: Duration,
    sink: &mpsc::Sender<Event>,
) {
    if spec.name.is_empty() {
        let _ = sink
            .send(Event::new(
                EVENT_ID_NONE,
                EventMsg::Error(ErrorEvent {
                    code: "tool_name_invalid".into(),
                    message: "serve: register_tools 收到空工具名,已跳过".into(),
                    details: None,
                }),
            ))
            .await;
        return;
    }
    if tools.get(&spec.name).is_some() {
        let _ = sink
            .send(Event::new(
                EVENT_ID_NONE,
                EventMsg::Error(ErrorEvent {
                    code: "tool_name_conflict".into(),
                    message: format!(
                        "serve: 工具名 '{}' 已被占用(内置或已注册远程工具),已跳过",
                        spec.name
                    ),
                    details: None,
                }),
            ))
            .await;
        return;
    }
    tools.register_with_source(
        ToolSource::Remote,
        Arc::new(RemoteTool::new(spec.clone(), bridge.clone(), timeout)),
    );
    tracing::info!(tool = %spec.name, "serve: 远程工具已注册");
}

/// env `REFLECT_REMOTE_TOOL_TIMEOUT_SECS` → 单次远程工具等待上限。
/// 非法值回退默认(120s)。
fn remote_tool_timeout_from_env() -> Duration {
    let secs = std::env::var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REMOTE_TOOL_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// 构造 serve 启动时的 `session_configured` 握手事件。
///
/// 与 submission_loop 首条 user_input 分支同款填充逻辑:provider 取
/// spec 前缀,context window 走 `[context_windows]` 覆盖表 →
/// `context_window_for` 回退表,并同步写共享句柄(供
/// `get_context_remaining` 工具读到同一值)。热重载切 model 后由
/// reload 通道重新 emit(不经本函数)。
fn proactive_session_configured(thread: &Arc<AgentThread>) -> Event {
    let cfg = thread.config();
    let model = cfg.current_model();
    let provider = model
        .split_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut sc = reflect_protocol::SessionConfiguredEvent::new(model.clone(), provider);
    // RwLock guard 不是 Send:先把值拷出来再构造事件(与 core 同款注释)。
    let override_hit = cfg.context_window_overrides.read().get(&model).copied();
    sc.context_window_size = override_hit.or_else(|| reflect_llm::context_window_for(&model));
    *cfg.context_window_size.write() = sc.context_window_size;
    Event::new(EVENT_ID_NONE, EventMsg::SessionConfigured(sc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 远程工具超时_env_解析与回退() {
        // 默认值。
        // SAFETY: 单测内短临界区设置 env(测试进程内串行执行)。
        unsafe {
            std::env::remove_var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS");
        }
        assert_eq!(remote_tool_timeout_from_env(), Duration::from_secs(120));
        unsafe {
            std::env::set_var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS", "5");
        }
        assert_eq!(remote_tool_timeout_from_env(), Duration::from_secs(5));
        unsafe {
            std::env::set_var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS", "not-a-number");
        }
        assert_eq!(remote_tool_timeout_from_env(), Duration::from_secs(120));
        unsafe {
            std::env::remove_var("REFLECT_REMOTE_TOOL_TIMEOUT_SECS");
        }
    }
}
