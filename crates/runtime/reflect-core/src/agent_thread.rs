//! `AgentThread` — 持有 submission 通道,以及每 turn / 每 session 的事件分发。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reflect_llm::SharedModelRegistry;
use reflect_protocol::{Event, Submission};
use reflect_tools::{Sanitizer, ToolExecutionQueue, ToolRegistry};

use crate::config::AgentConfig;
use crate::submission_loop::submission_loop;
use crate::turn::TurnHandle;

/// 单个 session 事件订阅者的容量。生命周期事件(`SessionConfigured`、
/// `ShutdownComplete`)出现频率低,小缓冲即可;慢订阅者会丢弃最早挂起的事件。
const SESSION_SUB_CAPACITY: usize = 16;

/// M1 thread —— 一次一个 submission。M2 支持并发 turn。
pub struct AgentThread {
    cfg: AgentConfig,
    registry: SharedModelRegistry,
    tools: Arc<ToolRegistry>,
    /// M6:由本结构持有,让 `register_hook` 能改动 queue 的 `HookEngine`。
    /// 同时以 `Arc` clone 传入 submission loop task。
    tools_queue: Arc<ToolExecutionQueue>,
    sub_tx: mpsc::Sender<Submission>,
    cancel: CancellationToken,
    /// `Submission.id` → 该 submission 的事件 channel。submission loop 在
    /// 回合结束时移除条目,随之 drop `Sender` 并关闭调用方
    /// `TurnHandle` 里的 `Receiver`。
    turn_subs: Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>>,
    /// M6:会话级订阅者。`SessionConfigured` 与 `ShutdownComplete`
    /// 除逐回合 channel 外也在这里扇出,让 TUI 无需绑定某个
    /// submission 也能收到它们。
    session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
}

impl AgentThread {
    /// 构造 `AgentThread`。
    ///
    /// `sanitizer` 控制工具输出密钥脱敏:
    /// - `Some(arc)` — 使用调用方注入的 sanitizer(典型来源:
    ///   `Sanitizer::from_config(&reflect_config::SanitizeSection)`)。
    /// - `None` — fallback 到 `Sanitizer::with_defaults()`(10 个默认
    ///   pattern + `[REDACTED]`),与历史行为一致。
    ///
    /// 加这个参数是为了把 `~/.reflect/config.toml [sanitize]` 段真正
    /// 接到 queue 内部 `Ok(Ok(_))` 分支的脱敏 pass 上 —— review 2026-06-30
    /// 之前的版本硬编码 `with_defaults`,用户的 `enabled = false` /
    /// `marker = "..."` / `extra_patterns = [...]` 全部死信。
    pub fn new(
        cfg: AgentConfig,
        registry: SharedModelRegistry,
        tools: Arc<ToolRegistry>,
        sanitizer: Option<Arc<Sanitizer>>,
        hook_engine: Option<Arc<reflect_hooks::HookEngine>>,
    ) -> Self {
        let cancel = cfg.cancel.clone();
        let (sub_tx, sub_rx) = mpsc::channel::<Submission>(64);
        let turn_subs: Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut base_ctx = cfg.shared_tool_context();
        base_ctx.cancel = cfg.cancel.clone();
        // v1.x:允许外部注入从 config.toml `[hooks]` 构建的 HookEngine(含
        // builtin hook + 插件 hook)。此前硬编码 `HookEngine::new()`,导致
        // `[hooks]` 配置整段死信。`None` 时回退空 engine(向后兼容)。
        let hook_engine = hook_engine.unwrap_or_else(|| Arc::new(reflect_hooks::HookEngine::new()));
        let mut tools_queue = match sanitizer {
            Some(arc) => {
                ToolExecutionQueue::with_sanitizer(tools.clone(), hook_engine, base_ctx, arc)
            }
            None => ToolExecutionQueue::with_defaults(tools.clone(), hook_engine, base_ctx),
        };
        // 注入会话级 permission_mode 共享句柄,让 PreToolUse hook(PlanModeGate)
        // 能读到运行时 `/mode` 切换后的实时模式。与 `ApprovalGate` 持有的句柄
        // 同源(cfg.permission_mode),保证 hook 与 gate 决策一致。
        tools_queue.set_session_permission_mode(cfg.permission_mode.clone());
        let tools_queue = Arc::new(tools_queue);

        // 启动 submission_loop(它持有 sub_rx 与扇出句柄)。
        let turn_subs_c = turn_subs.clone();
        let session_subs_c = session_subs.clone();
        let registry_c = registry.clone();
        let tools_c = tools.clone();
        let cfg_c = cfg.clone();
        let tools_queue_for_loop = tools_queue.clone();
        tokio::spawn(async move {
            submission_loop(
                sub_rx,
                turn_subs_c,
                session_subs_c,
                cfg_c,
                registry_c,
                tools_c,
                tools_queue_for_loop,
            )
            .await;
        });

        Self {
            cfg,
            registry,
            tools,
            tools_queue,
            sub_tx,
            cancel,
            turn_subs,
            session_subs,
        }
    }

    /// 提交一个 `Submission`,并获取对应的 `TurnHandle` 用于接收其事件。
    /// submission 的 `id` 作为每 turn 事件路由的 key。
    pub async fn submit(&self, sub: Submission) -> TurnHandle {
        let (tx, rx) = mpsc::channel::<Event>(64);
        self.turn_subs.lock().insert(sub.id.clone(), tx);
        // 转发 submission;若循环已退出(如 `Op::Shutdown` 已处理,
        // `sub_rx` 已 drop),send 立即返回 Err。
        let sub_id = sub.id.clone();
        if self.sub_tx.send(sub).await.is_err() {
            // 移除刚插入的 turn 条目并 drop 其 `Sender`:否则 map 中残留的
            // Sender 会让 per-turn channel 永不关闭,调用方在
            // `TurnHandle::next()` 上永久等待(典型触发路径:Shutdown 后
            // cron driver 仍经 `submission_sender()` 注入新 submission)。
            self.turn_subs.lock().remove(&sub_id);
        }
        TurnHandle::new(rx)
    }

    /// v1.2 P1-2:clone 一份 submission sender 给外部调度器(cron driver),
    /// 让它能向 agent loop 注入 `Submission::user_input`。sender 是
    /// `mpsc::Sender`(clone 廉价、与 `submit` 共享同一 channel);loop
    /// 关闭后 send 返回 Err,调用方应忽略。
    ///
    /// 典型用法:`reflect-exec` 在 `AgentThread` 构造后取 sender,注入
    /// `CronScheduler`,driver 到期时 `tx.send(Submission::user_input(p))`。
    pub fn submission_sender(&self) -> mpsc::Sender<Submission> {
        self.sub_tx.clone()
    }

    /// 订阅 thread 范围的生命周期事件(`SessionConfigured`、`ShutdownComplete`)。
    /// 返回的 receiver 在 thread 关闭时一并关闭。多个订阅者相互独立;
    /// 每个订阅者各收到一份副本。
    pub fn subscribe_session(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel::<Event>(SESSION_SUB_CAPACITY);
        self.session_subs.lock().push(tx);
        rx
    }

    pub fn config(&self) -> &AgentConfig {
        &self.cfg
    }

    pub fn registry(&self) -> &SharedModelRegistry {
        &self.registry
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// 在 thread 共享的 `HookEngine` 上注册 hook。hook 会接收
    /// tool queue 处理的每一条 `PreToolUse` / `PostToolUse` /
    /// `PostToolUseFailure` 事件。
    pub fn hook_engine(&self) -> Arc<reflect_hooks::HookEngine> {
        self.tools_queue.hook_engine().clone()
    }

    pub fn register_hook<H: reflect_hooks::Hook + 'static>(&self, hook: H) {
        self.tools_queue.register_hook(hook);
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }
}
