//! `submission_loop` — 消费 `Submission`,通过 4 节点 `StateGraph` 驱动
//! turn,并发出事件。
//!
//! M2:把 M1 的内联 LLM 循环换成真正的 `StateGraph::run`,使用
//! `nodes::model_call` / `nodes::tool_exec` / `nodes::check_stop`。
//! M3:hook 通过 `HookEngine` 触发。
//! M4:`pre_loop` 在每次 model call 之前运行 compaction + memory +
//! skills + prompt builder;`NodeContext` 携带 M4 依赖。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use reflect_hooks::{HookContext, HookEngine, HookEvent};
use reflect_llm::{ChatMessage, SharedModelRegistry, SharedQuotaTracker};
use reflect_protocol::{
    AbortReason, ContextCompactedEvent, ContextCompactedStrategy, Event, EventMsg, MessageRole,
    PermissionMode, PermissionModeChangedEvent, PlanApprovedEvent, PlanId, PlanReadyEvent,
    PlanRejectedEvent, PlanRequestEvent, ReasoningEffortMirror, RolloutRecord, RolloutRecorder,
    SessionConfiguredEvent, Submission, TokenCountEvent, TurnAbortedEvent, TurnCompleteEvent,
    TurnId, TurnStartedEvent, TurnStatus, UserInputItem,
};
use reflect_tools::{
    ApprovalGate, ApprovalWaiters, AskUserInputWaiters, AskUserQuestionWaiters, PlanApprovalGate,
    ToolExecutionQueue, ToolRegistry, complete_approval, complete_ask_user_input,
    complete_ask_user_question, complete_plan_approval,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_tasks::BackgroundTaskQueue;
use crate::config::{AgentConfig, M4Deps};
use crate::graph::StateGraph;
use crate::graph::state::AgentState;
use crate::steering_queue::{SteeringPriority, SteeringQueue};

/// 传递给每个图节点的轻量 clone 的回合级上下文。
#[derive(Clone)]
pub struct NodeContext {
    pub turn_id: TurnId,
    /// M5:会话生命周期内稳定的线程 id。由 `submission_loop` 设置一次,
    /// 跨回合复用,让 rollout recorder 能用同一 session 标记每条记录。
    pub session_id: reflect_protocol::ThreadId,
    /// 当前 turn 用的 model spec。v0.2.2 起改为 `Arc<RwLock<String>>`:
    /// 持 `AgentConfig.model` 的共享副本,`model_call` / `cache_break`
    /// 每次读最新值,使 `reflect-exec` 热重载切换 model 时下一个 LLM
    /// 请求立刻走新 model。`Arc` clone 廉价、锁粒度仅 RwLock<String>。
    pub model: Arc<parking_lot::RwLock<String>>,
    /// v1.0 多 Provider 路由:角色 → spec slot 的路由策略。`model_call`
    /// 入口用 `policy.resolve(Role::Main)` 拿到 spec,失败时由
    /// `ModelRegistry::next_for` 在 pool 内自动切下一个 credential。
    pub policy: Arc<reflect_llm::RoutingPolicy>,
    pub registry: SharedModelRegistry,
    pub hook_engine: Arc<HookEngine>,
    pub tools_queue: Arc<ToolExecutionQueue>,
    pub sub_id: String,
    pub cancel: CancellationToken,
    pub event_tx: mpsc::Sender<Event>,
    /// 本回合的初始消息(用户输入)。M4:在 `pre_loop` 末尾被
    /// state.messages 取代。
    pub messages: Vec<ChatMessage>,
    pub max_iterations: u32,
    /// M4 依赖。设为 Option 是为了让测试可以省略。
    pub m4: Option<M4Deps>,
    /// M5:从 `m4.recorder` 复制出的可选 recorder,让单个节点(尤其是
    /// `pre_loop`)能直接发 Compaction / Message 记录,而不必穿透 `m4`。
    pub recorder: Option<Arc<dyn RolloutRecorder>>,
    /// M6:回合级审批 gate。当 TUI / lib 客户端希望 `Prompt` 权限工具
    /// 需要确认时为 `Some`;headless `reflect-exec` 式免确认执行时为 `None`。
    pub approval_gate: Option<Arc<ApprovalGate>>,
    /// v1.x S4:当前会话的 reasoning effort(`Low` / `Medium` / `High`)。
    /// `model_call` 在入口拍快照构造 `ChatRequest::thinking`;中途
    /// `/effort` 切换影响下一轮而非当前轮。
    pub effort: Arc<parking_lot::RwLock<ReasoningEffortMirror>>,
    /// v1.2 P1-12:会话级累计 token 用量(跨 turn 累加)。`model_call`
    /// 每次调用后累加 `_usage`;预算检查与 `get_context_remaining` 工具读它。
    pub session_usage: Arc<parking_lot::RwLock<reflect_protocol::TokenUsage>>,
    /// v1.2 P1-12:会话级 token 预算硬上限(共享句柄,与 `AgentConfig`
    /// 同一把 RwLock,让热重载贯穿)。`None` = 仅靠 `max_iterations`;
    /// `Some(n)` = 累计 `total_tokens >= n` 时 `model_call` 提前返回 `None`。
    pub token_budget: Arc<parking_lot::RwLock<Option<u64>>>,
    /// v1.2 P1-12(已有-B):`/compact` 强制标志共享句柄。`Op::Compact` 置
    /// `true`,`pre_loop` 读 + 清零,据此强制运行 compactor(无视阈值)。
    pub force_compact_next: Arc<parking_lot::RwLock<bool>>,
    /// v1.2 P1:本地 Langfuse 式日志 sink。`None` = telemetry 关闭;
    /// `Some` = `model_call` / `tool_exec` / turn 开始结束都写 trace 事件
    /// 与 model-io 记录到 `~/.reflect/traces/`。turn 级 span 由
    /// `submission_loop` 的 `enter_turn` guard 自动管理(RAII)。
    pub telemetry: Option<Arc<reflect_telemetry::TelemetrySink>>,
    /// v1.2 P1:目标模式 controller。`None` = 未激活;turn 结束后
    /// submission_loop 调 `on_turn_end` 自校验 + 续作。
    pub goal: Option<Arc<reflect_goal::GoalController>>,
    /// v1.x 功能 7:token plan 配额追踪器。`None` = 不追踪(无 credential
    /// 声明配额);`Some` = `model_call` 在 LLM 调用成功后累计 usage,
    /// 耗尽时对该 credential 触发 cooldown,`next_for` 顺位切到下一个 plan。
    pub quota_tracker: Option<SharedQuotaTracker>,
    /// v1.x Plan mode:共享的 `PlanApprovalGate`,供 `tool_exec` 在
    /// LLM 调用 `EnterPlanModeTool` / `ExitPlanModeTool` 后直接注册
    /// waiter 并通过 `dispatch_plan_request` / `dispatch_plan_ready`
    /// 发出 `PlanRequest` / `PlanReady` 事件。`None` 表示未启用
    /// (测试 / 旧调用方),`tool_exec` 看到 `None` 会走 no-op 路径
    /// —— 仅记录 `ToolCallEnd`,不触发 plan mode 切换。
    pub plan_approval_gate: Option<Arc<PlanApprovalGate>>,
    /// v1.x Plan mode:session-wide subscriber channel。`tool_exec` 在
    /// 合成 plan mode 事件后,经 `fan_out_session` 同步广播给所有
    /// 会话级订阅者(TUI 多 tab / 持久化层 / 外接 dashboard)。
    /// `None` 表示无订阅者,跳过 fan_out。
    pub plan_session_subs: Option<Arc<Mutex<Vec<mpsc::Sender<Event>>>>>,
    /// v1.4 A2:会话级转向队列句柄。`pre_loop` 在每次(含 ToolExec 回环)
    /// 入口收割其中的 Now / Attachment 消息并注入 `state.messages` 尾部,
    /// 实现「回合中途边跑边改需求」—— 此前转向只在下一个 turn 边界
    /// 合并,正在跑的回合无法收到补充指示。`None`(测试 / 旧构造方)=
    /// 跳过收割,行为不变。
    pub steering_queue: Option<Arc<Mutex<SteeringQueue>>>,
    /// v1.5 R1:OS 沙箱覆盖(`ThreadSettingsOverrides.sandbox_policy` 每
    /// turn 下发)。`tool_exec` 经 ToolEventForwarder 透传给队列 →
    /// `ToolContext.os_sandbox` → BashTool。`None` = 跟随 env。
    pub sandbox_override: Option<bool>,
    /// v1.x Plan mode:共享 `AgentConfig`(整个 `cfg.clone()` —— 大部分字段是
    /// `Arc<RwLock<…>>`,clone 廉价)。`tool_exec` 在 dispatch plan
    /// 模式事件时通过它转发给 `spawn_plan_approval_waiter`,让审批
    /// 决策能真正翻转会话级 `permission_mode` 槽(否则会写到临时
    /// cfg 上,session 看不到)。
    pub cfg: AgentConfig,
}

#[allow(clippy::too_many_arguments)]
pub async fn submission_loop(
    mut sub_rx: mpsc::Receiver<Submission>,
    turn_subs: Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>>,
    session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    mut cfg: AgentConfig,
    registry: SharedModelRegistry,
    _tools: Arc<ToolRegistry>,
    tools_queue: Arc<ToolExecutionQueue>,
    background_tasks: Arc<BackgroundTaskQueue>,
) {
    // Hook engine 由 `tools_queue` 持有(M6)。此处不构造它;
    // queue 提供 `register_hook` 供调用方扩展。
    let hook_engine = tools_queue.hook_engine().clone();

    // M6:会话级审批状态。waiters map 被所有回合级 gate 共享,让
    // request_id(uuid)能直接路由,无需定位原始 gate。session_allow 集合
    // 让 ApproveForSession 决策跨回合持续生效。
    let approval_waiters: ApprovalWaiters = Arc::new(Mutex::new(std::collections::HashMap::new()));
    // v1.1.0 P1 #14:session-wide ask-user-question waiters。同 `approval_waiters`
    // 模式 —— per-turn gate 通过 `with_state(..., Some(qw.clone()))` 共享引用,
    // `Op::AskUserQuestionResponse` 进来时由 `complete_ask_user_question` 全局
    // 路由,不需要找原始 gate。
    let question_waiters: AskUserQuestionWaiters =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let user_input_waiters: AskUserInputWaiters =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let session_allow = Arc::new(Mutex::new(std::collections::HashSet::new()));
    // v1.x Plan mode:全局 `PlanApprovalGate` 持有所有 pending plan 切换
    // 请求的 oneshot。`Op::EnterPlanMode` / `Op::ExitPlanMode` 注册 waiter
    // 然后 emit `PlanRequest` / `PlanReady` 等待 TUI 回执;`Op::PlanApproval`
    // 投递决策后由 `complete_plan_approval` 唤醒 waiter。
    // 包裹在 `Arc` 内,让 `tool_exec` 节点(`NodeContext`)可以共享同一份
    // gate 状态(原 `PlanApprovalGate` 不实现 `Clone`)。
    let plan_approval_gate = Arc::new(PlanApprovalGate::new());
    // P2:session 级 steering 队列与后台任务注入队列。
    let steering_queue = Arc::new(Mutex::new(SteeringQueue::new()));
    // v1.4 A1:在飞回合表 —— turn 级取消令牌的登记处。每个 UserInput
    // turn spawn 前登记(turn_id → 从会话令牌派生的 child_token),
    // turn 任务退出时注销。`Op::Interrupt`(不带 child_id)对表中所有
    // 令牌执行 cancel,让正在 `graph.run()` 里跑的 turn 真正停下来
    // (模型流 / bash 击杀 / 审批等待都监听该令牌)—— 此前 Interrupt
    // 只发事件不取消,正在跑的 turn 打不断。会话级 `Op::Shutdown` 走
    // `cfg.cancel.cancel()`(父令牌),级联所有 child_token,不经本表。
    let active_turns: Arc<Mutex<HashMap<TurnId, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // 是否安装回合级 ApprovalGate。M6 v0:通过 `AgentConfig.approvals`
    // 标志(由 TUI / lib facade 设置)或 `REFLECT_APPROVALS=1` 环境变量
    // 显式开启。headless `reflect-exec` 保持关闭,让既有 JSONL 路径
    // 默认维持免确认行为。
    let approval_enabled = cfg.approvals
        || std::env::var("REFLECT_APPROVALS")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    // SessionStart hook 每线程只触发一次。
    //
    // v1.x:`session_id` 优先用外部注入(`cfg.session_id`),否则回退到内部
    // 新分配(历史行为)。外部注入让 recorder 文件名、`SessionMeta.session_id`
    // 与 `SessionConfigured` 报告的 id 三者一致(TUI / 持久化场景需要)。
    let session_id = cfg.session_id.unwrap_or_default();
    let _ = hook_engine
        .dispatch(&HookEvent::SessionStart {
            session_id,
            config: serde_json::json!({ "model": cfg.current_model() }),
        })
        .await;

    let mut session_emitted = false;
    // v1.x:从首条 Submission(通常是首条 UserInput)携带的 `workspace`
    // 字段捕获。GUI 主动注入当前激活工作区;CLI / 测试场景不指定 →
    // 后续 `cfg.current_workspace()` 作为回退。捕获后保持不变,即使
    // `set_workspace` 后续切换工作区也不影响已归属 session。
    let mut session_workspace: Option<String> = None;
    while let Some(sub) = sub_rx.recv().await {
        let turn_tx = turn_subs
            .lock()
            .remove(&sub.id)
            .unwrap_or_else(|| mpsc::channel(8).0);

        // 首条 Submission(无论 op 类型)就锁定 workspace —— 之后即使
        // 切 workspace 也不影响该 session 的归属。
        if session_workspace.is_none() {
            session_workspace = sub.workspace.clone();
        }

        match sub.op {
            reflect_protocol::Op::UserInput {
                items,
                thread_settings,
            } => {
                // v1.3 analytics:一个 turn 一个根 span,涵盖 pre_loop →
                // model_call → tool_exec → check_stop 全程,OTLP 后端
                // 看到的是嵌套 span 而非离散事件。
                let turn_span = tracing::info_span!(
                    "agent.turn",
                    sub_id = %sub.id,
                );
                let _turn_enter = turn_span.enter();
                // P2:合并 steering 队列中的 NOW / ATTACHMENT 消息。
                let mut merged_items = items;
                {
                    let mut sq = steering_queue.lock();
                    for msg in sq.drain_all() {
                        merged_items.extend(msg.items);
                    }
                }
                // P2:注入已完成的后台任务结果(作为 system 风格文本块)。
                for task in background_tasks.drain_completed() {
                    if let Some(result) = task.result {
                        merged_items.push(UserInputItem::Text {
                            text: format!(
                                "[background task {} ({}) completed]\n{}",
                                task.id, task.kind, result
                            ),
                        });
                    }
                }

                if !session_emitted {
                    let mut sc = SessionConfiguredEvent::new(
                        cfg.current_model(),
                        provider_of(&cfg.current_model()),
                    );
                    // v1.x:覆盖 `new()` 内部生成的随机 id,统一为 loop 的
                    // session_id —— 保证 SessionConfigured.session_id 与
                    // recorder 文件名 / SessionMeta.session_id 三者一致
                    // (GUI 按路由 id 预分配 session,依赖此一致性)。
                    sc.session_id = session_id;
                    // v1.x:填入模型上下文窗口(供 TUI 上下文用量条做分母)。
                    // 级联:config.toml `[context_windows]` per-model 覆盖表(优先)
                    // → 内置 `context_window_for` 静态回退表 → None。
                    // 让用户能为新/私端模型直接配窗口,无需改代码 + 重新编译。
                    // 注意:RwLock guard 不是 Send,先把值拷出来再 await。
                    let override_hit = cfg.context_window_overrides.read().get(&sc.model).copied();
                    sc.context_window_size =
                        override_hit.or_else(|| reflect_llm::context_window_for(&sc.model));
                    // v1.2 P1-12:同步写入共享句柄,让 `get_context_remaining`
                    // 工具读到同一值(热重载切 model 后此处刷新)。
                    *cfg.context_window_size.write() = sc.context_window_size;
                    let ev = Event::new(
                        reflect_protocol::EVENT_ID_NONE,
                        EventMsg::SessionConfigured(sc),
                    );
                    let _ = turn_tx.send(ev.clone()).await;
                    fan_out_session(&session_subs, &ev);
                    // M5:把会话头部持久化到 rollout recorder。
                    // v1.x:workspace 字段 —— 首条 Submission 携带的
                    // `workspace` 优先,否则回退到 `cfg.current_workspace()`
                    // 解析出的字符串(由 `set_workspace` 后台管理)。
                    if let Some(rec) = cfg.m4.as_ref().and_then(|m| m.recorder.clone()) {
                        let ws = session_workspace
                            .clone()
                            .or_else(|| cfg.current_workspace().to_str().map(|s| s.to_string()));
                        let _ = rec
                            .record(RolloutRecord::SessionMeta {
                                session_id,
                                model: cfg.current_model(),
                                started_at: chrono::Utc::now(),
                                workspace: ws,
                            })
                            .await;
                    }
                    session_emitted = true;
                }
                // 共享 `cfg.model` 的 Arc 副本,让后续 graph 节点的每次
                // LLM 调用都读最新值(配合热重载)。`thread_settings.model`
                // 仍是 String,这里做一次 String→Arc<RwLock> 适配。
                let model: Arc<parking_lot::RwLock<String>> = thread_settings
                    .model
                    .map(|s| Arc::new(parking_lot::RwLock::new(s)))
                    .unwrap_or_else(|| Arc::clone(&cfg.model));
                let cancel = cfg.cancel.clone();
                let sub_id = sub.id.clone();
                let turn_id = TurnId::new();
                // v1.4 A1:派生本回合专属 child_token —— 会话级取消
                // (`Op::Shutdown`)经父令牌自动级联,而 `Op::Interrupt`
                // 只取消本回合令牌,粒度从「会话」细化到「回合」。
                // 登记进在飞回合表供 Interrupt 路由;turn 任务退出时注销。
                let turn_cancel = cancel.child_token();
                active_turns.lock().insert(turn_id, turn_cancel.clone());
                let _ = turn_tx
                    .send(Event::new(
                        sub_id.clone(),
                        EventMsg::TurnStarted(TurnStartedEvent {
                            turn_id,
                            user_message_id: Some(uuid::Uuid::new_v4().to_string()),
                        }),
                    ))
                    .await;

                // v1.5 E1:UserPromptSubmit hook —— prompt 进模型前的最后
                // 一道用户可编程关卡。Deny 拒绝整个回合(Error + Abort,
                // prompt 不进模型也不落盘);InjectMessage 以
                // `<system-reminder>` 附加引导后照常执行。
                {
                    let hook_ctx = HookContext {
                        session_id,
                        turn_id,
                        workspace: cfg.current_workspace(),
                        permission_mode: cfg.permission_mode(),
                    };
                    let prompt_text = merged_items
                        .iter()
                        .filter_map(|i| match i {
                            UserInputItem::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let decision = hook_engine
                        .dispatch(&HookEvent::UserPromptSubmit {
                            text: prompt_text,
                            ctx: hook_ctx,
                        })
                        .await;
                    let resolved = decision.resolve();
                    if let Some(reason) = resolved.deny_reason {
                        tracing::info!(reason = %reason, "UserPromptSubmit denied; rejecting turn");
                        let _ = turn_tx
                            .send(Event::new(
                                sub.id.clone(),
                                EventMsg::Error(reflect_protocol::ErrorEvent {
                                    message: format!("prompt rejected by hook: {reason}"),
                                    code: "prompt_rejected".into(),
                                    details: Some(serde_json::json!({ "reason": reason })),
                                }),
                            ))
                            .await;
                        let _ = turn_tx
                            .send(Event::new(
                                sub.id.clone(),
                                EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id,
                                    reason: AbortReason::Error {
                                        code: "prompt_rejected".into(),
                                        message: reason,
                                    },
                                }),
                            ))
                            .await;
                        continue; // 跳过本回合:不落盘、不进模型。
                    }
                    if !resolved.injected.is_empty() {
                        let guidance = resolved
                            .injected
                            .iter()
                            .map(|m| m.content.clone())
                            .collect::<Vec<_>>()
                            .join("\n");
                        merged_items.push(UserInputItem::Text {
                            text: format!("<system-reminder>{guidance}</system-reminder>"),
                        });
                    }
                }

                // 由用户输入构造初始 message 列表。
                // v1.2 P0:Text + Image items 合并为同一个 UserContent blocks 数组,
                // 让 provider 收到原子化的图文消息。Text-only 单 item 路径与旧行为
                // 完全一致;LocalImage / Skill / QuestionAnswer 留待后续接通。
                let messages = user_input_items_to_messages(merged_items);

                // v1.x 每轮回填:有 recorder 时,每轮 UserInput 前从 rollout
                // 重建会话历史。放在写本轮 user 记录**之前** replay,历史里
                // 天然不含本轮输入,不会重复;也让 `Op::Rewind` 截断后下一轮
                // 自然从更短的 rollout 回放(rewind 分支注释声称的设计落地)。
                // 这修复了跨轮失忆:引擎除 preload-once 外此前无任何跨轮累积,
                // 第 2 轮起模型只看到新输入。
                // 无 recorder 的线程保留旧 preload-once 语义(resume 路径的
                // recorder 绑定旧 id,replay 已含全部历史,preload 不再叠加,
                // 避免重复 echo)。
                let base: Vec<ChatMessage> =
                    if let Some(rec) = cfg.m4.as_ref().and_then(|m| m.recorder.clone()) {
                        match rec.replay(session_id).await {
                            Ok(records) => crate::resume::records_to_preload(&records),
                            Err(e) => {
                                // 回填失败不致命:退化为无历史轮(与 recorder
                                // 缺席同款),本轮对话照常进行。
                                tracing::warn!("rollout replay 回填失败,退化为无历史轮: {e:#}");
                                Vec::new()
                            }
                        }
                    } else {
                        std::mem::take(&mut *cfg.preload_messages.write())
                    };

                // v1.2 P2:持久化本轮 user 输入(此前从未落盘)。
                // submission_loop 原先只在 turn 结束后写 assistant 的最后一条
                // 纯文本,user 消息全 workspace 无一处持久化,导致 session JSONL
                // 缺一半对话(磁盘实证:83 文件中 26 个零 message)。
                //
                // 此处只持久化「本轮新输入」(line 277 的 messages),严格在
                // preload 历史 extend 之前 —— 避免把 `--resume` 回放的历史 user
                // 消息重复落盘(那些历史已经在父 session 文件里了)。
                //
                // 数据格式:把 llm 层 ContentBlock(仅 Text/Image 两变体)映射成
                // protocol 层 ContentBlock,统一存为 Vec<protocol ContentBlock>
                // 的 JSON 数组 —— 与 assistant 落盘格式对称,让 resume / fork
                // 能无损还原(含图文混排)。
                if let Some(rec) = cfg.m4.as_ref().and_then(|m| m.recorder.clone()) {
                    let user_blocks: Vec<reflect_protocol::ContentBlock> = messages
                        .iter()
                        .filter_map(|m| match m {
                            ChatMessage::User(uc) => Some(uc.blocks.iter().cloned()),
                            _ => None,
                        })
                        .flatten()
                        .map(|b| match b {
                            reflect_llm::ContentBlock::Text { text } => {
                                reflect_protocol::ContentBlock::Text { text }
                            }
                            reflect_llm::ContentBlock::Image { data, mime_type } => {
                                reflect_protocol::ContentBlock::Image { data, mime_type }
                            }
                        })
                        .collect();
                    if !user_blocks.is_empty() {
                        let _ = rec
                            .record(RolloutRecord::message(
                                turn_id,
                                MessageRole::User,
                                serde_json::to_value(&user_blocks)
                                    .unwrap_or(serde_json::Value::Null),
                            ))
                            .await;
                    }
                }

                // v1.x resume / 每轮回填:把上面算出的 base 历史(recorder
                // replay 或 preload-once)前置到当前用户输入之前。`pre_loop`
                // 会在本 turn 首次进入时把 `ctx.messages` 整体 seed 进
                // `state.messages`,因此历史 + 新输入会一起进入会话上下文。
                let messages = if base.is_empty() {
                    messages
                } else {
                    let mut v = base;
                    v.extend(messages);
                    v
                };

                // v1.x:全局迭代上限(env/TOML 解析),可被激活 agent 的
                // `max_turns` 进一步收紧(取 `min`,即更严格的优先)。
                let global_max = cfg.current_max_iterations();
                let max_iterations = cfg
                    .m4
                    .as_ref()
                    .and_then(|m| m.active_agent_def.max_turns)
                    .map_or(global_max, |n| n.min(global_max));

                // v1.5 R1:ThreadSettingsOverrides 诚实化 —— 三个死字段
                // 全部消费:
                // - max_tool_concurrency:热调工具执行队列并发上限(每 turn 可变);
                // - approval_policy:Prompt/Deny 强制本回合启用审批门
                //   (Deny 额外置 deny-all,需审批工具一律拒);Auto 跟随会话;
                // - sandbox_policy:OS 沙箱覆盖(OsSandbox=强制开,
                //   WorkspaceOnly/FullAccess=关 OS 层,文件工具路径检查不受影响)。
                if let Some(mc) = thread_settings.max_tool_concurrency {
                    tools_queue.set_max_concurrency(mc);
                    tracing::info!(
                        max_concurrency = mc,
                        "thread_settings: tool concurrency adjusted"
                    );
                }
                let turn_approval_enabled = match thread_settings.approval_policy {
                    Some(reflect_protocol::ApprovalPolicy::Prompt) => {
                        tracing::info!("thread_settings: approval forced on (prompt)");
                        true
                    }
                    Some(reflect_protocol::ApprovalPolicy::Deny) => {
                        tracing::info!("thread_settings: approval deny-all for this turn");
                        true
                    }
                    Some(reflect_protocol::ApprovalPolicy::Auto) | None => approval_enabled,
                };
                let sandbox_override = thread_settings
                    .sandbox_policy
                    .map(|p| matches!(p, reflect_protocol::SandboxPolicy::OsSandbox));

                let ctx = NodeContext {
                    turn_id,
                    session_id,
                    model: model.clone(),
                    policy: cfg.policy.clone(),
                    registry: registry.clone(),
                    hook_engine: hook_engine.clone(),
                    tools_queue: tools_queue.clone(),
                    sub_id: sub_id.clone(),
                    // v1.4 A1:回合级令牌(非会话级)—— `Op::Interrupt`
                    // 经在飞回合表 cancel 它;`Op::Shutdown` 经父令牌级联。
                    // 对 graph 各节点而言与旧会话令牌语义兼容(select! /
                    // kill_on_cancel 不变)。
                    cancel: turn_cancel.clone(),
                    event_tx: turn_tx.clone(),
                    messages,
                    max_iterations,
                    m4: cfg.m4.clone(),
                    recorder: cfg.m4.as_ref().and_then(|m| m.recorder.clone()),
                    // v1.x S4:`/effort` 切换的读取源。`Clone` 后多副本共享
                    // 同一把 RwLock,`model_call` 在入口拍快照,单轮 LLM
                    // 调用不受中途 `/effort` 切换影响。
                    effort: cfg.effort.clone(),
                    // v1.2 P1-12:会话级 token 用量 / 预算。与 `cfg` 共享同一把
                    // RwLock(`Arc` clone),`model_call` 写、预算检查 /
                    // get_context_remaining 工具读。
                    session_usage: cfg.session_usage.clone(),
                    token_budget: cfg.token_budget.clone(),
                    force_compact_next: cfg.force_compact_next.clone(),
                    telemetry: cfg.telemetry.clone(),
                    goal: cfg.goal.clone(),
                    quota_tracker: cfg.quota_tracker.clone(),
                    // v1.x Plan mode:把 submission_loop 拥有的共享
                    // `PlanApprovalGate` 与 session subscriber list clone
                    // 进去,让 `tool_exec` 在 LLM 调 EnterPlanMode /
                    // ExitPlanMode 工具时能直接注册 waiter + emit
                    // `PlanRequest` / `PlanReady`(无须额外 message
                    // 通道把工具结果转发回主循环)。
                    plan_approval_gate: Some(plan_approval_gate.clone()),
                    plan_session_subs: Some(session_subs.clone()),
                    // v1.4 A2:共享会话级转向队列,pre_loop 每次入口收割。
                    steering_queue: Some(steering_queue.clone()),
                    // v1.5 R1:每回合沙箱覆盖。
                    sandbox_override,
                    // 同上:把 `cfg.clone()` 传下去,让
                    // `spawn_plan_approval_waiter` 能在 user approve
                    // 后翻转会话级 `permission_mode` 槽。
                    cfg: cfg.clone(),
                    approval_gate: turn_approval_enabled.then(|| {
                        let g = ApprovalGate::with_state(
                            turn_tx.clone(),
                            sub_id.clone(),
                            approval_waiters.clone(),
                            session_allow.clone(),
                            cfg.permission_resolver.clone(),
                            Some(question_waiters.clone()),
                            Some(user_input_waiters.clone()),
                            Some(cfg.permission_mode.clone()),
                        );
                        // P2 `yolo-classifier`:把 cfg 的启发式分类器注入 gate。
                        if cfg.yolo_classifier.is_some() {
                            g.set_yolo_classifier(
                                cfg.yolo_classifier.clone(),
                                Some(cfg.yolo_threshold),
                            );
                        }
                        // v1.5 R1:approval_policy = deny → 回合级 deny-all。
                        if matches!(
                            thread_settings.approval_policy,
                            Some(reflect_protocol::ApprovalPolicy::Deny)
                        ) {
                            g.set_deny_all(true);
                        }
                        Arc::new(g)
                    }),
                };

                let state = AgentState::default();
                let graph = StateGraph::new(state, ctx);

                // M6:把 turn spawn 成 tokio 任务,让 submission 循环
                // 能继续处理 Op::ToolApproval / Op::HookApproval /
                // Op::Interrupt / Op::Shutdown 等提交,即使 turn 正在
                // 经 `ApprovalGate` 等待用户输入。原先同步的
                // `graph.run().await` 会让任何审批流程死锁。
                let turn_tx_clone = turn_tx.clone();
                let sub_id_clone = sub_id.clone();
                let recorder = cfg.m4.as_ref().and_then(|m| m.recorder.clone());
                // v1.2 P1:goal / steering / telemetry 句柄 clone 进 spawned task,
                // 供 turn 结束后自校验 + 续作。
                let cfg_goal_clone = cfg.goal.clone();
                let steering_queue_clone = steering_queue.clone();
                let cfg_telemetry_clone = cfg.telemetry.clone();
                // v1.4 A1:取消判定三件套 —— 回合令牌(被 Interrupt cancel)、
                // 会话令牌(被 Shutdown cancel,级联回合令牌)、在飞回合表
                // (终态注销)。区分两者才能只对「用户中断」发 TurnAborted,
                // Shutdown 已有自己的 ShutdownComplete 事件。
                // v1.4 C1:本线程自己的子代理状态槽(若本线程是子代理)。
                // 回合结束后写迭代数与 token 用量,父会话查询即时可见。
                let status_slot = cfg.subagent_status.clone();
                let turn_cancel_clone = turn_cancel.clone();
                let session_cancel_clone = cfg.cancel.clone();
                let active_turns_clone = active_turns.clone();
                // v1.2 P1:turn 级 telemetry span(RAII guard,drop 时自动写
                // `turn.completed` + duration)。guard 在 spawned task 内创建,
                // 这样 drop 时机 = turn 真正完成(而非 spawn 时刻)。
                let telemetry_sink = cfg.telemetry.clone();
                tokio::spawn(async move {
                    let turn_span_guard = telemetry_sink
                        .as_ref()
                        .map(|sink| sink.enter_turn(&turn_id.to_string()));
                    let final_state = graph.run().await;
                    // 显式标完成(带 status),防止 guard 只写兜底的 note。
                    if let Some(g) = &turn_span_guard {
                        let status = if final_state.completed_normally {
                            reflect_telemetry::SpanStatus::Completed
                        } else {
                            reflect_telemetry::SpanStatus::Failed
                        };
                        g.complete(
                            status,
                            serde_json::json!({
                                "iteration": final_state.iteration,
                                "input_tokens": final_state.total_usage.input_tokens,
                                "output_tokens": final_state.total_usage.output_tokens,
                                "total_tokens": final_state.total_usage.total_tokens,
                            }),
                        );
                    }

                    // v1.2 P1-12:预算耗尽也算 turn 完成(发 TurnComplete,
                    // 状态 `TokenBudgetExceeded`),区别于 `completed_normally`
                    // (自然结束 / MaxIterations)。两者都走 turn-completion 路径;
                    // 异常取消(error / cancel)仍走 `!completed_normally &&
                    // !budget_exceeded` 的静默丢弃分支。
                    if final_state.completed_normally || final_state.budget_exceeded {
                        // v1.x S4:把 `model.read()` 的 String 快照先 clone 出来,
                        // 否则 RwLockReadGuard 跨 .await → future !Send。
                        // pricing 表 lookup 不需要 RwLock 持有,只要 model 字符串。
                        let model_for_pricing = model.read().clone();
                        let _ = turn_tx_clone
                            .send(Event::new(
                                sub_id_clone.clone(),
                                EventMsg::TokenCount(TokenCountEvent {
                                    input_tokens: final_state.total_usage.input_tokens,
                                    output_tokens: final_state.total_usage.output_tokens,
                                    cached_tokens: final_state.total_usage.cached_tokens,
                                    cache_write_tokens: final_state.total_usage.cache_write_tokens,
                                    total_tokens: final_state.total_usage.total_tokens,
                                    // v1.x S4:用当前 model spec + total_usage 算
                                    // per-turn USD cost;model 不在 pricing 表里
                                    // (未知 / 本地无标价的 model) → None,TUI
                                    // 显示 "$—" 而非 "$0.00"(保守)。
                                    cost_usd: reflect_llm::providers::pricing::price(
                                        &model_for_pricing,
                                        &final_state.total_usage,
                                    ),
                                    ..Default::default()
                                }),
                            ))
                            .await;
                        // v1.2 P1-12:turn 状态优先级 —— 预算耗尽 >
                        // MaxIterations > Success。预算耗尽时 model_call
                        // 提前返回 None,iteration 通常 < 32,故需显式判
                        // budget_exceeded 才不会误报 Success。
                        // GAIA-fix: MaxIterations 改由 `hit_max_iterations`
                        // 标志判断(由 `model_call` 触发上限时置位),而非
                        // 硬编码 `iteration > 32`——后者在配置上限 < 32
                        // (如 GAIA 的 20)时会把"用尽迭代未作答"误报为
                        // `Success`,既掩盖真因又丢失诊断信号。
                        let status = if final_state.budget_exceeded {
                            TurnStatus::TokenBudgetExceeded
                        } else if final_state.hit_max_iterations {
                            TurnStatus::MaxIterations
                        } else {
                            TurnStatus::Success
                        };
                        // v1.2 P1:goal 自校验需本轮 token 数 —— 在
                        // `final_state.total_usage` 被 move 进 TurnComplete 前捕获。
                        let goal_turn_tokens = final_state.total_usage.total_tokens as u64;
                        // v1.4 C1:子代理自报告 —— 迭代数 + token 用量
                        // (在 total_usage 被 move 进 TurnComplete 之前)。
                        if let Some(slot) = &status_slot {
                            slot.set_iteration(final_state.iteration);
                            slot.add_tokens(goal_turn_tokens);
                        }
                        let _ = turn_tx_clone
                            .send(Event::new(
                                sub_id_clone,
                                EventMsg::TurnComplete(TurnCompleteEvent {
                                    turn_id,
                                    usage: final_state.total_usage,
                                    status,
                                }),
                            ))
                            .await;
                        // 提取本轮 assistant 的最后一条非空纯文本(goal 自校验
                        // 用)。注意:此提取不再与 recorder 绑定 —— 原先它嵌在
                        // `if let Some(rec) = recorder` 内,导致无 recorder 时
                        // goal 校验被静默跳过(既有耦合 bug,顺带修正)。
                        let last_assistant_text = final_state
                            .latest_content
                            .iter()
                            .find_map(|b| match b {
                                reflect_protocol::ContentBlock::Text { text } => {
                                    if text.is_empty() {
                                        None
                                    } else {
                                        Some(text.clone())
                                    }
                                }
                                _ => None,
                            })
                            .unwrap_or_default();

                        // M5/v1.2 P2:持久化 assistant 本轮完整 ContentBlocks
                        // (Text + ToolUse + ToolResult),让 resume / fork 能无损
                        // 回放含工具调用的对话。此前只存最后一条纯文本,工具调用
                        // 链全丢;纯工具调用收尾的 turn(last_assistant_text 为
                        // 空)则整条不写,导致大量 session 文件零 message。
                        //
                        // `latest_content` 含本轮最后一次 model_call 产生的
                        // Text + ToolUse,加上 tool_exec 追加的 ToolResult。
                        // content 用 protocol ContentBlock 数组的 JSON 序列化,
                        // resume 端按数组反序列化还原(blocks 形态,而非裸字符串)。
                        if let Some(rec) = recorder {
                            if !final_state.latest_content.is_empty() {
                                let _ = rec
                                    .record(RolloutRecord::message(
                                        turn_id,
                                        MessageRole::Assistant,
                                        serde_json::to_value(&final_state.latest_content)
                                            .unwrap_or(serde_json::Value::Null),
                                    ))
                                    .await;
                            }
                        }

                        // v1.2 P1:目标模式 —— turn 结束后自校验。
                        // work_context = 本轮 assistant 最后的文本。
                        if let Some(goal) = &cfg_goal_clone {
                            let tokens = goal_turn_tokens;
                            match goal.on_turn_end(&last_assistant_text, tokens).await {
                                Ok(tr) => {
                                    if let Some(prompt) = tr.continuation_prompt {
                                        // 推 steering 续作(下个 turn 自动开)。
                                        let items = reflect_goal::continuation_to_items(&prompt);
                                        steering_queue_clone.lock().push(
                                            crate::steering_queue::SteeringPriority::Attachment,
                                            items,
                                        );
                                    }
                                    // telemetry:记录 goal.turn.verified。
                                    if let Some(sink) = &cfg_telemetry_clone {
                                        sink.record_goal_event(
                                            Some(&turn_id.to_string()),
                                            "goal.turn.verified",
                                            reflect_telemetry::Level::Info,
                                            serde_json::json!({
                                                "verdict": match &tr.verdict {
                                                    reflect_goal::GoalVerdict::Met { .. } => "met",
                                                    reflect_goal::GoalVerdict::Unmet { .. } => "unmet",
                                                    reflect_goal::GoalVerdict::Blocked { .. } => "blocked",
                                                },
                                                "status": tr.status.as_str(),
                                                "goal_turn": final_state.iteration,
                                                "tokens_used": tokens,
                                            }),
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "goal verification failed; continuing");
                                }
                            }
                        }
                    } else if turn_cancel_clone.is_cancelled()
                        && !session_cancel_clone.is_cancelled()
                    {
                        // v1.4 A1:回合被 `Op::Interrupt` 取消(非 Shutdown
                        // 级联)—— 用**真实** turn_id 发中止事件。旧实现由
                        // Interrupt 分支现编一个 `TurnId::new()`,客户端无法
                        // 把事件对应到实际在飞的回合。
                        let _ = turn_tx_clone
                            .send(Event::new(
                                sub_id_clone,
                                EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id,
                                    reason: AbortReason::UserInterrupt,
                                }),
                            ))
                            .await;
                        // 中断可能留下半截助手消息:把已产出的内容完整落盘
                        //(`latest_content` 可含孤儿 tool_use —— resume 端
                        // `records_to_preload` 会过滤无 result 的在飞工具对,
                        // 保证恢复不因半对消息报错),复用正常路径的格式。
                        if let Some(rec) = recorder {
                            if !final_state.latest_content.is_empty() {
                                let _ = rec
                                    .record(RolloutRecord::message(
                                        turn_id,
                                        MessageRole::Assistant,
                                        serde_json::to_value(&final_state.latest_content)
                                            .unwrap_or(serde_json::Value::Null),
                                    ))
                                    .await;
                            }
                        }
                    }
                    // v1.4 A1:turn 已终态(完成 / 取消 / 异常),统一注销
                    // 在飞回合表条目。陈旧条目(极端时序下 Interrupt 先到)
                    // 的 remove 是幂等 no-op。
                    active_turns_clone.lock().remove(&turn_id);
                });
            }
            reflect_protocol::Op::Compact => {
                // v1.2 P1-12(已有-B):`/compact` 设置 `force_compact_next`
                // 标志,让下一个 turn 的 `pre_loop` 强制运行 compactor(无视
                // trigger_tokens 阈值)。架构原因:`Op::Compact` 到达时无活跃
                // turn / 无可压缩 messages,真正的压缩只能在下一个 turn 的
                // `pre_loop`(有 messages 时)做。
                cfg.request_force_compact();
                tracing::info!(
                    "/compact: force_compact_next queued; next turn's pre_loop will compact"
                );
                // emit 一个 Noop event 让 TUI 知道请求已被接受(下一轮
                // pre_loop 才会 emit 真实的 Microcompact/SmartPrune event)。
                let _ = turn_tx
                    .send(Event::new(
                        sub.id,
                        EventMsg::ContextCompacted(ContextCompactedEvent {
                            strategy: ContextCompactedStrategy::Noop,
                            removed_messages: 0,
                            before_tokens: 0,
                            after_tokens: 0,
                        }),
                    ))
                    .await;
            }
            reflect_protocol::Op::Interrupt { child_id } => {
                // v1.x Plan mode:把 abort reason 同步写到 `AgentConfig` 上,
                // 让下一次 turn 的 `pre_loop` 一次性消费并注入 ephemeral
                // system block("## Previous Turn"),提醒 LLM 上轮被中断、
                // 在 Plan mode 下应当收尾并调 `ExitPlanMode`。仅在当前 mode
                // 是 Plan 时这条 hint 才有意义 —— 但写入总是无副作用,让
                // `pre_loop` 自行决定是否消费(避免在这里再读一次锁)。
                cfg.set_last_abort_reason(AbortReason::UserInterrupt);
                if let Some(child) = child_id {
                    // v1.4 A1(原 B3 预留):定向中断单个子代理。查子代理
                    // 运行注册表并 cancel 对应令牌;被中断子代理的
                    // `TurnAborted`(带其真实 turn_id)由它自己的 spawn 任务
                    // 在取消路径 emit,这里不重复发。父会话不受影响。
                    let hit = cfg
                        .subagent_runtime
                        .as_ref()
                        .is_some_and(|reg| reg.cancel_child(&child));
                    if hit {
                        tracing::info!(child_id = %child, "subagent interrupt dispatched");
                    } else {
                        tracing::warn!(
                            child_id = %child,
                            "interrupt targeted unknown/finished subagent; no-op"
                        );
                    }
                } else {
                    // v1.4 A1:真中断 —— 对所有在飞回合的 turn 级令牌执行
                    // cancel。graph 各节点(模型流 select! / bash
                    // kill_on_cancel / 审批等待)收到取消信号后尽快收尾,
                    // 回合任务在 `!completed_normally` 分支用**真实 turn_id**
                    // emit `TurnAborted`。此前实现只发一条现编 turn_id 的
                    // 事件、正在跑的 turn 照常跑完,中断名存实亡。
                    let cancelled: Vec<TurnId> = {
                        let mut turns = active_turns.lock();
                        turns
                            .drain()
                            .map(|(id, tok)| {
                                tok.cancel();
                                id
                            })
                            .collect()
                    };
                    if cancelled.is_empty() {
                        // 无在飞回合(空闲期按 Esc):保持旧回执行为 ——
                        // 发一条事件让客户端知道请求被接受;此时没有真实
                        // turn_id 可引用,沿用新生成 id 的历史约定。
                        let _ = turn_tx
                            .send(Event::new(
                                sub.id,
                                EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id: TurnId::new(),
                                    reason: AbortReason::UserInterrupt,
                                }),
                            ))
                            .await;
                    } else {
                        tracing::info!(
                            turns = ?cancelled,
                            "interrupt cancelled in-flight turn(s)"
                        );
                    }
                }
            }
            // 批次十九 → 批次二十二:`Op::Rewind` —— 对话回退。
            // 现在真正做持久化截断:调 `RolloutRecorder::truncate_after` 删除
            // 目标 turn(含)及之后的 JSONL 记录(`JsonlRolloutWriter` 会先写
            // `.bak` 备份,可恢复)。引擎的会话内多轮 history 每 turn 重建,
            // 截断后下一次 turn 自然从更短的 rollout 回放 —— 故此处不持有
            // 内存 history 也能正确回退。`None` = 回退到最后一个 turn(最常用)。
            // truncate 失败不致命:warn 后降级为纯事件回执(旧行为)。
            reflect_protocol::Op::Rewind { to_turn_id } => {
                let target = to_turn_id
                    .as_deref()
                    .and_then(|s| reflect_protocol::TurnId::parse_str(s).ok());
                let dropped = match cfg.m4.as_ref().and_then(|m| m.recorder.clone()) {
                    Some(rec) => rec.truncate_after(target.as_ref()).await.unwrap_or_else(|e| {
                        tracing::warn!("rollout truncate_after failed, falling back to event-only rewind: {e:#}");
                        0
                    }),
                    None => 0,
                };
                let _ = turn_tx
                    .send(Event::new(
                        sub.id,
                        EventMsg::TurnRewound(reflect_protocol::TurnRewoundEvent {
                            to_turn_id: to_turn_id.clone(),
                            truncated_after: dropped,
                        }),
                    ))
                    .await;
            }
            reflect_protocol::Op::Shutdown => {
                // 先触发取消令牌:任何正在 graph.run() 里跑的 turn
                // (LLM 流 / 工具执行 / 审批等待)都会收到取消信号并尽快收尾,
                // 而不是被 detached 后继续空转或挂死。此前漏了这一步,导致
                // Shutdown 后 spawned turn 任务成为孤儿(其 turn_tx 已被本
                // 循环 drop,send 永久失败却无人 await)。
                cfg.cancel.cancel();
                let ev = Event::new(sub.id, EventMsg::ShutdownComplete);
                let _ = turn_tx.send(ev.clone()).await;
                fan_out_session(&session_subs, &ev);
                break;
            }
            // v1.4 A2:回合中途转向入口。客户端(界面 / SDK)对正在跑的
            // 回合投喂补充指示:push 进会话转向队列,正在跑的 turn 会在
            // 下一次 pre_loop(ToolExec 回环入口)收割注入;若无在飞
            // turn,消息留队,下一个 UserInput turn 边界合并(既有行为)。
            reflect_protocol::Op::Steer { priority, items } => {
                let p = match priority {
                    reflect_protocol::SteeringPriorityMirror::Now => SteeringPriority::Now,
                    reflect_protocol::SteeringPriorityMirror::Attachment => {
                        SteeringPriority::Attachment
                    }
                };
                tracing::info!(
                    priority = ?p,
                    items = items.len(),
                    "steer: mid-turn steering queued"
                );
                steering_queue.lock().push(p, items);
            }
            // v1.4 C1:子代理状态查询 —— 从状态中心取快照(全 clone,不
            // 阻塞任何子代理),SubagentStatus 事件经 per-turn 通道 + 会话
            // 扇出双路送达。`child_id = None` 列出全部,`Some` 定向。
            reflect_protocol::Op::QuerySubagents { child_id } => {
                let children = cfg
                    .subagent_runtime
                    .as_ref()
                    .map(|reg| reg.snapshot(child_id.as_deref()))
                    .unwrap_or_default();
                if child_id.is_some() && children.is_empty() {
                    tracing::debug!(?child_id, "query_subagents: no matching child");
                }
                let ev = Event::new(
                    sub.id.clone(),
                    EventMsg::SubagentStatus(reflect_protocol::SubagentStatusEvent { children }),
                );
                let _ = turn_tx.send(ev.clone()).await;
                fan_out_session(&session_subs, &ev);
            }
            reflect_protocol::Op::ToolApproval { id, decision }
            | reflect_protocol::Op::HookApproval { id, decision } => {
                // M6:把用户裁决交给等待中的 ApprovalGate。
                // 若 waiter 已被移除(超时 / 取消)则返回 false —— 静默丢弃。
                let resolved = complete_approval(&approval_waiters, &id, decision);
                if !resolved {
                    tracing::debug!(
                        request_id = %id,
                        "approval reply arrived but no waiter; dropping"
                    );
                }
            }
            // ── v1.x Plan mode:完整状态机 ─────────────────────────────
            // 流程:
            // 1. 生成 `PlanId`,在 `plan_approval_gate` 注册 oneshot waiter
            // 2. emit `PlanRequest` / `PlanReady` 等待 TUI 用户审批
            // 3. 阻塞在 `rx.await`;cancel token 触发或 caller 退出 → Deny
            // 4. 决策 approve → flip `PermissionMode` + emit `PermissionModeChanged`
            //    决策 deny → emit `PlanRejected { reason }`
            // 5. `Op::PlanApproval { id, decision }` 由后续轮询的 `match` arm
            //    投递决策(`complete_plan_approval`)唤醒本 waiter
            //
            // v1.x 修复:两条 Op 路径统一走 `dispatch_plan_request` /
            // `dispatch_plan_ready` helper,与 `tool_exec` 在 LLM 调
            // `EnterPlanModeTool` / `ExitPlanModeTool` 后的合成路径
            // 共享同一份代码 —— 避免「LLM 触发但 `Op::ExitPlanMode`
            // 路径永远没被走,placeholder 永远暴露」的 drift。
            reflect_protocol::Op::EnterPlanMode { task } => {
                dispatch_plan_request(
                    &plan_approval_gate,
                    &session_subs,
                    turn_tx.clone(),
                    sub.id.clone(),
                    cfg.clone(),
                    task,
                )
                .await;
            }
            reflect_protocol::Op::ExitPlanMode => {
                // Op 路径(`/exit-plan` slash)走的是「无 LLM 工具调用」语境,
                // 没有 markdown 可用;给一个**确定性安全回退**而不是
                // 历史占位符,既避免误导用户,也方便回归测试断言。
                dispatch_plan_ready(
                    &plan_approval_gate,
                    &session_subs,
                    turn_tx.clone(),
                    sub.id.clone(),
                    cfg.clone(),
                    FALLBACK_PLAN_MARKDOWN.to_string(),
                )
                .await;
            }
            // v1.x Plan mode:`Op::PlanApproval` 由 TUI 在 modal 上按 1/2/3 后
            // 发出,把选择投递给对应 `PlanId` 的 waiter。`id` 是 String
            // (序列化稳定),parse 到 `PlanId` 才能查到 waiter。
            reflect_protocol::Op::PlanApproval {
                id: plan_id_str,
                choice,
            } => match plan_id_str.parse::<PlanId>() {
                Ok(plan_id) => {
                    let resolved =
                        complete_plan_approval(plan_approval_gate.waiters(), &plan_id, choice);
                    if !resolved {
                        tracing::debug!(plan_id = %plan_id, "plan approval reply arrived but no waiter; dropping");
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        plan_id = %plan_id_str,
                        error = %e,
                        "malformed plan_id in Op::PlanApproval; dropping"
                    );
                }
            },
            // v1.x S4:`/effort low|medium|high` slash 的 Op 路径。写
            // `cfg.effort` 槽(下一轮 `model_call` 读最新值构造
            // `ChatRequest::thinking`)。不 emit 任何 Event —— slash 派发
            // 端已直接推 Pill 反馈用户;这里只做审计行 + 写槽。
            reflect_protocol::Op::SetEffort { effort } => {
                tracing::info!(
                    ?effort,
                    "effort override applied; will take effect on next model_call"
                );
                cfg.set_effort(effort);
            }
            // v1.1.0 P1 #14:TUI 在 question modal 上提交 / 取消时,投递结构化
            // 答案回阻塞的 `ask_user_question` 工具。`request_id` 配对
            // `AskUserQuestionEvent.request_id`;`answers` 由 TUI 从
            // `PendingQuestion.selected` + `custom` 序列化得到,用户按 Esc 时
            // 为空 `AskUserAnswer`。
            reflect_protocol::Op::AskUserQuestionResponse { id, answers } => {
                let resolved = complete_ask_user_question(&question_waiters, &id, answers);
                if !resolved {
                    tracing::debug!(
                        request_id = %id,
                        "ask_user_question reply arrived but no waiter; dropping"
                    );
                }
            }
            reflect_protocol::Op::AskUserInputResponse { id, text } => {
                let resolved = complete_ask_user_input(&user_input_waiters, &id, text);
                if !resolved {
                    tracing::debug!(
                        request_id = %id,
                        "ask_user reply arrived but no waiter; dropping"
                    );
                }
            }
            reflect_protocol::Op::SetPermissionMode { mode } => {
                // v1.3 safety baseline(plan §四):旧版 `Bypass` 模式
                // 仍可经历史 config 快照与残留的 client RPC 触达,
                // 但已不再是合法的终端用户权限状态。当协议层下发
                // `PermissionMode::Bypass`(无论来源是过期配置还是
                // 直接调用)时,统一降级为 `Prompt` 并发出一次性
                // migration 通知 —— 绝不静默授予 blanket bypass 语义。
                // `Bypass` 仅在 protocol 枚举中保留以维持序列化兼容。
                let target = sanitize_permission_mode_for_baseline(mode);
                if matches!(target, reflect_protocol::PermissionMode::Prompt) && mode != target {
                    use reflect_protocol::{
                        EVENT_ID_NONE, Event, EventMsg, PermissionModeChangedEvent,
                    };
                    let ev = Event::new(
                        EVENT_ID_NONE,
                        EventMsg::PermissionModeChanged(PermissionModeChangedEvent {
                            from: mode,
                            to: target,
                        }),
                    );
                    fan_out_session(&session_subs, &ev);
                    tracing::warn!(
                        requested = ?mode,
                        effective = ?target,
                        "downgraded legacy PermissionMode::Bypass to Prompt per v1.3 safety baseline"
                    );
                }
                apply_permission_mode_change(&cfg, target, &session_subs, &turn_subs);
            }
            reflect_protocol::Op::CyclePermissionMode => {
                let next = cfg.permission_mode().next_in_ui_cycle();
                apply_permission_mode_change(&cfg, next, &session_subs, &turn_subs);
            }
            // v1.2 P1:目标模式 —— 构造 GoalController 放入 cfg.goal。
            // 校验用 LLM client 从 registry 取主模型的 client(与主 agent 同模型,
            // 除非 [goal].verification_model 指定 —— 后者后续接入)。
            reflect_protocol::Op::EnterGoalMode {
                goal,
                verify_command,
                token_budget,
            } => {
                // 与 `model_call` 节点同源的 spec 选取:policy 主 slot 的
                // primary 优先;为空时(TUI bootstrap 未注入 routing policy,
                // 如 `reflect tui` 直接启动)回退当前激活 model spec ——
                // 否则 `next_for("")` 恒为 None,goal 模式被静默丢弃
                // (warn 后 continue,校验永不发生)。
                let mut spec = cfg
                    .policy
                    .resolve(reflect_llm::policy::Role::Main)
                    .primary
                    .clone();
                if spec.is_empty() {
                    spec = cfg.current_model();
                }
                let client = match registry.next_for(&spec, &[]) {
                    Some(nc) => nc.client.clone(),
                    None => {
                        tracing::warn!("goal: no LLM client available for verification");
                        continue;
                    }
                };
                let controller = Arc::new(reflect_goal::GoalController::new(
                    &goal,
                    verify_command,
                    token_budget,
                    client,
                    cfg.cancel.clone(),
                    cfg.telemetry.clone(),
                ));
                cfg.goal = Some(controller);
                tracing::info!(goal = %goal, "goal mode entered");
            }
            // v1.2 P1:退出目标模式 —— 清空 controller。
            reflect_protocol::Op::ExitGoalMode => {
                if let Some(c) = cfg.goal.take() {
                    c.clear();
                }
                tracing::info!("goal mode exited");
            }
            // v1.3 SDK:`RegisterTools` / `ToolExecutionResponse` 是 serve
            // 模式的**进程内控制 Op**,由 `reflect serve` 的 stdin 循环就
            // 地处理,正常不进 submission_loop。误入时(如 exec 转发 /
            // 未来其它入口)安全忽略并留审计日志,不影响 turn 状态。
            reflect_protocol::Op::RegisterTools { .. }
            | reflect_protocol::Op::ToolExecutionResponse { .. } => {
                tracing::debug!(
                    op = sub.op.discriminant(),
                    "serve-local op reached core loop; ignored"
                );
            }
        }

        drop(turn_tx);
    }
}

/// v1.3 安全基线(plan §四):legacy `PermissionMode::Bypass` 仅因序列化
/// 兼容保留在协议枚举中(旧配置 / 已保存会话仍可能序列化出该值)。
/// 它**绝不**是合法的运行时目标。在每个入口点 —— `Op::SetPermissionMode`
/// 以及未来任何经配置解析为 `Bypass` 的路径 —— 一律降级为最严格的等价值,
/// 让"一揽子绕过"语义无法借旧快照回潮。
fn sanitize_permission_mode_for_baseline(
    requested: reflect_protocol::PermissionMode,
) -> reflect_protocol::PermissionMode {
    match requested {
        reflect_protocol::PermissionMode::Bypass => reflect_protocol::PermissionMode::Prompt,
        other => other,
    }
}

fn provider_of(model_spec: &str) -> String {
    model_spec
        .split_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 把会话级事件扇出给所有订阅者。用 `try_send`,让慢速 / 无界消费的
/// 订阅者绝不阻塞 submission loop;channel 满或已关闭时静默丢弃事件
/// (订阅者会看到一段空缺,语义等同丢掉一帧 UI 渲染)。
fn fan_out_session(subs: &Mutex<Vec<mpsc::Sender<Event>>>, ev: &Event) {
    let guard = subs.lock();
    for tx in guard.iter() {
        let _ = tx.try_send(ev.clone());
    }
}

/// v1.x:best-effort 把一条 [`RolloutRecord`] 落进 session JSONL。
///
/// 与 `submission_loop` 里 user/assistant 消息落盘同款写法:从 `cfg.m4.recorder`
/// 取 writer,失败只 `tracing::warn!` 不阻断主流程。用 `tokio::spawn` 解耦,
/// 让**同步**调用点(如 [`apply_permission_mode_change`])也能触发持久化而
/// 无需改签名;plan dispatch 等已在 async 上下文的调用点同样适用。
///
/// 顺序保证:同一 turn 内的多条 record 可能在 spawn task 里乱序落盘。
/// 但 plan / mode 切换是低频、语义独立的事件,乱序不影响 JSONL 还原
/// (每条 record 自带 `at` 时间戳,消费方可按时序重排)。
fn persist_rollout_best_effort<F>(cfg: &AgentConfig, build: F)
where
    F: FnOnce() -> RolloutRecord,
{
    if let Some(rec) = cfg.m4.as_ref().and_then(|m| m.recorder.clone()) {
        let record = build();
        tokio::spawn(async move {
            if let Err(e) = rec.record(record).await {
                tracing::warn!(error = %e, "rollout: failed to persist plan/mode record");
            }
        });
    }
}

/// v1.1.0 P1:`/mode` / Shift+Tab / `Op::SetPermissionMode` 写入 mode 并广播。
fn apply_permission_mode_change(
    cfg: &AgentConfig,
    to: PermissionMode,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    turn_subs: &Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>>,
) {
    let from = cfg.permission_mode();
    if from == to {
        return;
    }
    cfg.set_permission_mode(to);
    let ev = Event::new(
        reflect_protocol::EVENT_ID_NONE,
        EventMsg::PermissionModeChanged(PermissionModeChangedEvent { from, to }),
    );
    fan_out_session(session_subs, &ev);
    let guard = turn_subs.lock();
    for tx in guard.values() {
        let _ = tx.try_send(ev.clone());
    }
    // v1.x:把 mode 切换落进 session JSONL(best-effort)。与 EventMsg 的
    // 广播不同,持久化是为了让 `--resume` / `/export` 能还原权限状态轨迹。
    // 单点覆盖 /mode slash、CyclePermissionMode、Bypass→Prompt 降级三条路径。
    persist_rollout_best_effort(&cfg, || RolloutRecord::PermissionModeChanged {
        from,
        to,
        at: chrono::Utc::now(),
    });
    tracing::info!(?from, ?to, "permission mode changed");
}

// ── v1.x Plan mode:公共 mode-flip 纯函数 ──────────────────────────
//
// 主循环 spawn 版 waiter 与 tool_exec 阻塞版 dispatch 共用,避免 mode-flip
// 逻辑在两处漂移。两条路径只在「如何拿到 choice」上不同(spawn vs await),
// 落地动作(set permission_mode + 广播 / emit PlanRejected)完全一致。

/// Plan 审批的方向。决定同一个 [`PlanApprovalChoice`] 落到哪个目标 mode。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanDirection {
    /// `ExitPlanMode`:批准后离开 Plan 模式进入执行。
    /// `AutoMode` → `AcceptEdits`;`ManualApprove` → `Prompt`。
    Exit,
    /// `EnterPlanMode`:批准后进入 Plan 模式(两种 choice 都切 `Plan`)。
    Enter,
}

/// 把用户审批决策落到会话级 `permission_mode` 并广播给所有订阅者。
///
/// - `AutoMode` / `ManualApprove` → 按 `direction` 计算 `target_mode`,
///   `cfg.set_permission_mode(target)` + 广播 `PermissionModeChanged`
/// - `Revise` → `Exit` 方向 emit `PlanRejected`(让 TUI 关 modal、焦点回输入框);
///   `Enter` 方向静默(不进入 plan 模式)
///
/// 已处于目标 mode 时 no-op(重入保护)。不 persist mode 切换 —— 与原 spawn
/// waiter 行为一致(`apply_permission_mode_change` 的 persist 只覆盖 slash 路径)。
#[allow(clippy::too_many_arguments)]
async fn apply_plan_approval_choice(
    choice: reflect_protocol::PlanApprovalChoice,
    plan_id: PlanId,
    direction: PlanDirection,
    cfg: &AgentConfig,
    sub_id: &str,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
) {
    use reflect_protocol::PlanApprovalChoice as C;
    match choice {
        C::AutoMode | C::ManualApprove => {
            let target_mode = match direction {
                PlanDirection::Exit => match choice {
                    C::AutoMode => PermissionMode::AcceptEdits,
                    C::ManualApprove => PermissionMode::Prompt,
                    C::Revise => unreachable!("Revise 已在下方分支处理"),
                },
                PlanDirection::Enter => PermissionMode::Plan,
            };
            let from = cfg.permission_mode();
            if from == target_mode {
                // 已处于目标 mode(罕见但可能 —— 重入 EnterPlanMode)。
                tracing::debug!(
                    plan_id = %plan_id,
                    ?from,
                    "plan approval approved but already in target mode; no-op"
                );
                // no-op 路径同样要发关闭事件:Exit 方向的 plan_approval
                // 提示条靠 `PlanApproved` 事件关闭(TUI 不做乐观关闭),
                // 缺了它提示条会挂死。
                if matches!(direction, PlanDirection::Exit) {
                    fan_out_session(
                        session_subs,
                        &Event::new(
                            sub_id,
                            EventMsg::PlanApproved(PlanApprovedEvent { plan_id }),
                        ),
                    );
                }
                return;
            }
            cfg.set_permission_mode(target_mode);
            let ev = Event::new(
                sub_id,
                EventMsg::PermissionModeChanged(PermissionModeChangedEvent {
                    from,
                    to: target_mode,
                }),
            );
            fan_out_session(session_subs, &ev);
            // Exit 方向(ExitPlanMode 审批)补发 `PlanApproved`:TUI 的
            // plan_approval 提示条不乐观关闭,靠此事件「关闭提示条 + 推
            // ✓ Plan approved 确认行」同帧完成。协议里该事件早已定义
            // (与 PlanRejected 配对),此前引擎从未 emit —— 确认行是死代码。
            if matches!(direction, PlanDirection::Exit) {
                fan_out_session(
                    session_subs,
                    &Event::new(
                        sub_id,
                        EventMsg::PlanApproved(PlanApprovedEvent { plan_id }),
                    ),
                );
            }
            tracing::info!(
                plan_id = %plan_id,
                ?from,
                ?target_mode,
                "plan mode transition committed"
            );
        }
        C::Revise => match direction {
            PlanDirection::Exit => {
                // 留在 plan 模式,不改 permission mode;emit `PlanRejected`
                // 让 TUI 关闭 modal 并把焦点还给输入框(用户输入反馈继续 plan)。
                tracing::info!(
                    plan_id = %plan_id,
                    "plan approval: user chose to revise (stay in plan mode)"
                );
                let reject_reason = Some("user wants to revise the plan".to_string());
                persist_rollout_best_effort(cfg, || RolloutRecord::PlanRejected {
                    plan_id,
                    reason: reject_reason.clone(),
                    at: chrono::Utc::now(),
                });
                let ev = Event::new(
                    sub_id,
                    EventMsg::PlanRejected(PlanRejectedEvent {
                        plan_id,
                        reason: reject_reason,
                    }),
                );
                fan_out_session(session_subs, &ev);
            }
            PlanDirection::Enter => {
                tracing::info!(
                    plan_id = %plan_id,
                    "plan enter: user chose not to enter plan mode"
                );
                // Enter 的 Revise = 不进入,留当前 mode;无事件。
            }
        },
    }
}

/// v1.x Plan mode:把 plan approval 决策路由到 `PermissionMode` 翻转。
///
/// 由 `Op::EnterPlanMode` / `Op::ExitPlanMode` 触发:
/// - 阻塞在 `rx.await` 等待 `Op::PlanApproval` 的决策
/// - `Approve` / `ApproveForSession` → `cfg.set_permission_mode(target)` +
///   emit `EventMsg::PermissionModeChanged { from, to }`
/// - `Deny { reason }` → emit `EventMsg::PlanRejected { plan_id, reason }`
/// - rx channel 关闭(cancel / caller drop)→ 静默退出,不发事件
///
/// 函数 spawn 到独立 task,因为 `submission_loop` 的主循环要立即回到
/// `sub_rx.recv()` 处理后续 `Op::PlanApproval`,而 plan 决策可能在很久
/// 之后(用户离开键盘几小时)才到。
#[allow(clippy::too_many_arguments)]
fn spawn_plan_approval_waiter(
    rx: tokio::sync::oneshot::Receiver<reflect_protocol::PlanApprovalChoice>,
    plan_id: PlanId,
    cfg: AgentConfig,
    sub_id: String,
    session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
) {
    tokio::spawn(async move {
        let choice = match rx.await {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!(
                    plan_id = %plan_id,
                    "plan approval waiter cancelled (caller dropped or session shutdown)"
                );
                return;
            }
        };
        apply_plan_approval_choice(
            choice,
            plan_id,
            PlanDirection::Exit,
            &cfg,
            &sub_id,
            &session_subs,
        )
        .await;
    });
}

/// EnterPlan 路径(`dispatch_plan_request`)的简化 waiter:审批通过 → 切到
/// `Plan` 模式;否则留在原模式。与 `spawn_plan_approval_waiter` 分开,因为
/// EnterPlan 的语义是"请求进入 plan 模式"(目标固定 Plan),而非 ready 路径
/// 的"选择如何执行"(AutoMode/ManualApprove/Revise)。
fn spawn_plan_enter_waiter(
    rx: tokio::sync::oneshot::Receiver<reflect_protocol::PlanApprovalChoice>,
    plan_id: PlanId,
    cfg: AgentConfig,
    sub_id: String,
    session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
) {
    tokio::spawn(async move {
        let choice = match rx.await {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!(
                    plan_id = %plan_id,
                    "plan enter waiter cancelled (caller dropped or session shutdown)"
                );
                return;
            }
        };
        apply_plan_approval_choice(
            choice,
            plan_id,
            PlanDirection::Enter,
            &cfg,
            &sub_id,
            &session_subs,
        )
        .await;
    });
}

/// v1.x Plan mode:LTM 通过 `Op::EnterPlanMode` 或 LLM 通过
/// `EnterPlanModeTool` 进入 Plan mode 的统一入口。
///
/// 1. 生成 `PlanId` 并在 `plan_approval_gate` 注册 oneshot waiter
///    —— 必须在 emit 事件**之前**注册,以防 TUI 在事件到达前已发出
///    `Op::PlanApproval` 导致 decision 丢失。
/// 2. emit `PlanRequest { task }` 给 TUI 弹 modal 审批。
/// 3. `spawn_plan_approval_waiter` 阻塞等待用户决策,审批通过才真正
///    翻转 `PermissionMode`。
///
/// 该 helper 由 `submission_loop` 主循环的 `Op::EnterPlanMode` 分支
/// 与 `tool_exec` 在检测到 `EnterPlanModeTool` 成功调用后共用,确保
/// 两条路径走完全相同的「注册 → emit → spawn waiter」序列。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_plan_request(
    gate: &Arc<PlanApprovalGate>,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    turn_tx: mpsc::Sender<Event>,
    sub_id: String,
    cfg: AgentConfig,
    task: String,
) {
    let plan_id = PlanId::new();
    let rx = gate.register(plan_id);
    // v1.x:把 PlanRequest 落进 session JSONL,让 resume / export 能还原 plan 起点。
    let request_at = chrono::Utc::now();
    persist_rollout_best_effort(&cfg, || RolloutRecord::PlanRequest {
        plan_id,
        task: task.clone(),
        at: request_at,
    });
    if turn_tx
        .send(Event::new(
            sub_id.clone(),
            EventMsg::PlanRequest(PlanRequestEvent {
                plan_id,
                task: task.clone(),
            }),
        ))
        .await
        .is_err()
    {
        tracing::warn!(
            plan_id = %plan_id,
            "turn_tx dropped before PlanRequest could be delivered; waiter will be cancelled"
        );
        // 不需要显式 cancel:oneshot::Receiver drop 后 rx.await 返回 Err,
        // waiter 静默退出。
        return;
    }
    tracing::info!(plan_id = %plan_id, %task, "plan request emitted");
    spawn_plan_enter_waiter(rx, plan_id, cfg, sub_id, session_subs.clone());
}

/// v1.x Plan mode:LLM / slash 退出 Plan mode 的统一入口。
///
/// `markdown` 是已经过 fallback 解析的真实字符串(`tool_exec` 路径会
/// 优先用 `args["markdown"]` → `state.latest_content` 末段 assistant
/// 文本 → [`FALLBACK_PLAN_MARKDOWN`];slash 路径只到 fallback)。行为
/// 与 [`dispatch_plan_request`] 对称。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_plan_ready(
    gate: &Arc<PlanApprovalGate>,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    turn_tx: mpsc::Sender<Event>,
    sub_id: String,
    cfg: AgentConfig,
    markdown: String,
) {
    let plan_id = PlanId::new();
    let rx = gate.register(plan_id);
    // 把 plan 落盘成持久产物(`<workspace>/.reflect/plan/<plan_id>.md`)。
    // best-effort:失败返回 None,不阻断下面的 PlanReady 发送。
    let plan_path = persist_plan_markdown(&cfg.current_workspace(), plan_id, &markdown);
    // v1.x:同步把 PlanReady(markdown 全文 + path)落进 session JSONL。
    // 与 .md 双写:JSONL 让 session 完整可还原 / 可 export,.md 给 LLM cat 引用。
    let ready_at = chrono::Utc::now();
    let ready_path = plan_path.clone();
    let ready_markdown = markdown.clone();
    persist_rollout_best_effort(&cfg, || RolloutRecord::PlanReady {
        plan_id,
        markdown: ready_markdown,
        path: ready_path,
        at: ready_at,
    });
    if turn_tx
        .send(Event::new(
            sub_id.clone(),
            EventMsg::PlanReady(PlanReadyEvent {
                plan_id,
                markdown: markdown.clone(),
                path: plan_path,
            }),
        ))
        .await
        .is_err()
    {
        tracing::warn!(
            plan_id = %plan_id,
            "turn_tx dropped before PlanReady could be delivered; waiter will be cancelled"
        );
        return;
    }
    tracing::info!(
        plan_id = %plan_id,
        markdown_chars = markdown.len(),
        "plan ready emitted"
    );
    spawn_plan_approval_waiter(rx, plan_id, cfg, sub_id, session_subs.clone());
}

// ── v1.x Plan mode:tool_exec 专用阻塞版 dispatch ────────────────────
//
// 与上方 spawn 版的关键差异:`tool_exec` 是 agent 图节点,返回值决定图的走向。
// 若像主循环那样把 waiter spawn 到后台再立即返回,agent 图会继续推进
// (PreLoop → ModelCall),在 plan 未批准时就开始写代码 —— 这正是本修复要消除
// 的 bug。因此图节点路径**必须同步 await** 用户决策。
//
// 两条路径共用 [`apply_plan_approval_choice`] 落地 mode 切换,保证语义一致。

/// `tool_exec` 检测到 LLM 调 `ExitPlanModeTool` 成功后,改调本函数:
/// emit `PlanReady` 后**同步 await** 用户 `Op::PlanApproval` 决策,绝不 spawn。
///
/// 配合 `cfg.cancel` 保证 session Shutdown 时能退出,避免图节点永久挂起。
/// 返回用户 `choice`;`None` = waiter 被 cancel 或 `turn_tx` 提前 drop。
/// `tool_exec` 拿到任意返回后照常走 `PreLoop` —— 此时 mode 已翻转
/// (Approve)或仍处 Plan(Revise / cancel),下一轮按新 mode 自然推进。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_plan_ready_blocking(
    gate: &Arc<PlanApprovalGate>,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    turn_tx: mpsc::Sender<Event>,
    sub_id: String,
    cfg: AgentConfig,
    markdown: String,
) -> Option<reflect_protocol::PlanApprovalChoice> {
    let plan_id = PlanId::new();
    let rx = gate.register(plan_id);
    // plan 落盘(与 spawn 版一致,best-effort;失败降级为 None path)。
    let plan_path = persist_plan_markdown(&cfg.current_workspace(), plan_id, &markdown);
    let ready_at = chrono::Utc::now();
    let ready_path = plan_path.clone();
    let ready_markdown = markdown.clone();
    persist_rollout_best_effort(&cfg, || RolloutRecord::PlanReady {
        plan_id,
        markdown: ready_markdown,
        path: ready_path,
        at: ready_at,
    });
    if turn_tx
        .send(Event::new(
            sub_id.clone(),
            EventMsg::PlanReady(PlanReadyEvent {
                plan_id,
                markdown: markdown.clone(),
                path: plan_path,
            }),
        ))
        .await
        .is_err()
    {
        tracing::warn!(
            plan_id = %plan_id,
            "turn_tx dropped before PlanReady could be delivered; blocking dispatch aborted"
        );
        return None;
    }
    tracing::info!(
        plan_id = %plan_id,
        markdown_chars = markdown.len(),
        "plan ready emitted (blocking); awaiting user decision in tool_exec"
    );
    // clone 一份 cancel 句柄,避免 select! 两个分支同时借用 cfg。
    let cancel = cfg.cancel.clone();
    tokio::select! {
        c = rx => match c {
            Ok(choice) => {
                apply_plan_approval_choice(
                    choice, plan_id, PlanDirection::Exit,
                    &cfg, &sub_id, session_subs,
                ).await;
                Some(choice)
            }
            Err(_) => {
                tracing::debug!(
                    plan_id = %plan_id,
                    "blocking plan waiter cancelled (caller dropped or session shutdown)"
                );
                None
            }
        },
        _ = cancel.cancelled() => {
            // session Shutdown 触发 cfg.cancel.cancel()。Op::Interrupt 不触发
            // 会话级 token(不可逆)—— 与普通 tool approval 阻塞行为一致,
            // 属既有架构局限,不在此修复范围。
            tracing::info!(
                plan_id = %plan_id,
                "session cancelled while awaiting plan approval; aborting blocking dispatch"
            );
            None
        }
    }
}

/// `tool_exec` 检测到 LLM 调 `EnterPlanModeTool` 成功后,改调本函数:
/// emit `PlanRequest` 后同步 await 用户决策。语义对称 [`dispatch_plan_ready_blocking`]。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_plan_request_blocking(
    gate: &Arc<PlanApprovalGate>,
    session_subs: &Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    turn_tx: mpsc::Sender<Event>,
    sub_id: String,
    cfg: AgentConfig,
    task: String,
) -> Option<reflect_protocol::PlanApprovalChoice> {
    let plan_id = PlanId::new();
    let rx = gate.register(plan_id);
    let request_at = chrono::Utc::now();
    persist_rollout_best_effort(&cfg, || RolloutRecord::PlanRequest {
        plan_id,
        task: task.clone(),
        at: request_at,
    });
    if turn_tx
        .send(Event::new(
            sub_id.clone(),
            EventMsg::PlanRequest(PlanRequestEvent {
                plan_id,
                task: task.clone(),
            }),
        ))
        .await
        .is_err()
    {
        tracing::warn!(
            plan_id = %plan_id,
            "turn_tx dropped before PlanRequest could be delivered; blocking dispatch aborted"
        );
        return None;
    }
    tracing::info!(
        plan_id = %plan_id,
        %task,
        "plan request emitted (blocking); awaiting user decision in tool_exec"
    );
    let cancel = cfg.cancel.clone();
    tokio::select! {
        c = rx => match c {
            Ok(choice) => {
                apply_plan_approval_choice(
                    choice, plan_id, PlanDirection::Enter,
                    &cfg, &sub_id, session_subs,
                ).await;
                Some(choice)
            }
            Err(_) => None,
        },
        _ = cancel.cancelled() => None,
    }
}

/// v1.x Plan mode:`Op::ExitPlanMode`(slash `/exit-plan`)与
/// `tool_exec` 在 LLM 调 `ExitPlanModeTool` 但**省略 markdown**
/// 时共用的确定性回退。**不**显示 placeholder 之类的非确定性
/// 字符串 —— 让 TUI / 集成测试可以稳定断言。
pub(crate) const FALLBACK_PLAN_MARKDOWN: &str = "(plan markdown not provided — pass `markdown` to ExitPlanModeTool or supply plan content in your prior assistant turn)";

/// plan markdown 落盘目录,相对于工作区根(`<workspace>/.reflect/plan/`)。
///
/// 与 `reflect-rollout` 的 `DEFAULT_ROLLOUT_DIR = ".reflect/sessions"`、
/// `reflect-telemetry` 的 `DEFAULT_TRACES_DIR = ".reflect/traces"` 同级,
/// 让 plan 成为可引用、可 `cat` 的持久产物(而非只在内存里飘一次的事件载荷)。
/// 注:`detect_project_root` 已把 `.reflect/` 列为项目根标记,此目录天然契合。
pub(crate) const DEFAULT_PLAN_DIR: &str = ".reflect/plan";

/// 把 plan markdown 落盘到 `<workspace>/.reflect/plan/<plan_id>.md`。
///
/// best-effort:写盘失败只 `tracing::warn!` 并返回 `None`,**不**阻断
/// `PlanReady` 事件发送——渲染永远不依赖落盘成功(降级为旧的无文件行为)。
/// `create_dir_all` 保证 `.reflect/plan/` 缺失时自动建出。
///
/// 这是 core 侧的框架行为(像 sessions/traces 那样自动落盘),不经
/// `PlanModeGate` hook,因此不受 Plan 模式只读约束。
fn persist_plan_markdown(
    workspace: &std::path::Path,
    plan_id: PlanId,
    markdown: &str,
) -> Option<std::path::PathBuf> {
    let dir = workspace.join(DEFAULT_PLAN_DIR);
    let path = dir.join(format!("{plan_id}.md"));
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, markdown)) {
        Ok(()) => {
            tracing::info!(plan_id = %plan_id, path = %path.display(), "plan markdown persisted");
            Some(path)
        }
        Err(e) => {
            tracing::warn!(plan_id = %plan_id, error = %e, "failed to persist plan markdown");
            None
        }
    }
}

/// v1.2 P0:把一批 `UserInputItem` 转成原子化的用户消息。
///
/// 所有 Text / Image / File item 合并进同一个 `UserContent.blocks` 数组,
/// 保证 provider 收到图文混排而非多个割裂的连续 user role。Text-only 单
/// item 与旧实现完全一致。尚未接通的 LocalImage / Skill / QuestionAnswer
/// 暂时跳过。
///
/// v1.x 新增 `File` 分支 —— 把文件 mention 展开为带路径标注的文本块
/// (`@<path>` 或 `@<path>:L<start>-L<end>`)。真实读取由 LLM `/read`
/// 工具按需触发(避免无谓 IO 与抽象泄露)。
fn user_input_items_to_messages(items: Vec<UserInputItem>) -> Vec<ChatMessage> {
    let blocks: Vec<reflect_llm::ContentBlock> = items
        .into_iter()
        .filter_map(|item| match item {
            UserInputItem::Text { text } => Some(reflect_llm::ContentBlock::Text { text }),
            UserInputItem::Image { data, mime_type } => {
                Some(reflect_llm::ContentBlock::Image { data, mime_type })
            }
            UserInputItem::File { path, range } => {
                let annotation = match range {
                    Some(r) => format!("@{}:L{}-L{}", path, r.start_line, r.end_line),
                    None => format!("@{}", path),
                };
                Some(reflect_llm::ContentBlock::Text { text: annotation })
            }
            UserInputItem::LocalImage { .. }
            | UserInputItem::Skill { .. }
            | UserInputItem::QuestionAnswer { .. } => None,
        })
        .collect();
    if blocks.is_empty() {
        Vec::new()
    } else {
        vec![ChatMessage::User(reflect_llm::UserContent { blocks })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_of_strips_model_name() {
        assert_eq!(provider_of("openai/gpt-4o"), "openai");
        assert_eq!(provider_of("claude-3"), "unknown");
    }

    #[test]
    fn user_input_text_remains_single_user_text_block() {
        let messages = user_input_items_to_messages(vec![UserInputItem::Text {
            text: "hello".into(),
        }]);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            ChatMessage::User(content) => {
                assert_eq!(content.blocks.len(), 1);
                match &content.blocks[0] {
                    reflect_llm::ContentBlock::Text { text } => assert_eq!(text, "hello"),
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn user_input_text_and_image_become_ordered_blocks() {
        let png = vec![0x89, 0x50, 0x4e, 0x47];
        let messages = user_input_items_to_messages(vec![
            UserInputItem::Text {
                text: "describe this".into(),
            },
            UserInputItem::Image {
                data: png.clone(),
                mime_type: "image/png".into(),
            },
        ]);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            ChatMessage::User(content) => {
                assert_eq!(content.blocks.len(), 2);
                assert!(matches!(
                    &content.blocks[0],
                    reflect_llm::ContentBlock::Text { text } if text == "describe this"
                ));
                match &content.blocks[1] {
                    reflect_llm::ContentBlock::Image { data, mime_type } => {
                        assert_eq!(data, &png);
                        assert_eq!(mime_type, "image/png");
                    }
                    other => panic!("expected image block, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn image_only_input_becomes_user_image_message() {
        let messages = user_input_items_to_messages(vec![UserInputItem::Image {
            data: vec![1, 2, 3],
            mime_type: "image/jpeg".into(),
        }]);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            ChatMessage::User(content) => assert!(matches!(
                &content.blocks[0],
                reflect_llm::ContentBlock::Image { mime_type, .. } if mime_type == "image/jpeg"
            )),
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_items_alone_produce_no_messages() {
        let messages = user_input_items_to_messages(vec![UserInputItem::LocalImage {
            path: std::path::PathBuf::from("x.png"),
        }]);
        assert!(messages.is_empty());
    }

    // ── v1.x Plan 模式:计划请求分发 ──
    //
    // 验证两条统一入口(slash `Op` 路径 + LLM tool_exec 路径共享)在调用后:
    // 1. 在 `plan_approval_gate` 注册了 waiter(可被 `complete_plan_approval`
    //    解析回 decision,即 oneshot 存在)。
    // 2. 经 `turn_tx` 发出 `PlanRequest { task }` / `PlanReady { plan_id, markdown }`
    //    事件,且 markdown 是真实传入的字符串而非 placeholder。
    //
    // 不验证模式翻转 / waiter task 唤醒 —— 那是 `spawn_plan_approval_waiter`
    // 的职责,已在 TUI reducer snapshot + unit 测试覆盖。

    fn test_cfg() -> AgentConfig {
        // 最小可用的 `AgentConfig`。`spawn_plan_approval_waiter` 持有 cfg 但
        // 仅在 user 决策到达后才读写 `permission_mode` —— 测试在收到事件后
        // 立即 drop cfg / waiters,waiter task 因 oneshot drop 而静默退出,
        // 不会真正触发 mode 翻转。
        AgentConfig::new("anthropic/x", "/tmp")
    }

    #[tokio::test]
    async fn dispatch_plan_request_emits_plan_request_and_registers_waiter() {
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let cfg = test_cfg();

        dispatch_plan_request(
            &gate,
            &session_subs,
            turn_tx,
            "sub-1".to_string(),
            cfg,
            "refactor auth".to_string(),
        )
        .await;

        // 事件应到达 turn_tx,且为 PlanRequest { task }。
        let ev = turn_rx.recv().await.expect("PlanRequest event missing");
        match ev.msg {
            EventMsg::PlanRequest(req) => {
                assert_eq!(req.task, "refactor auth");
            }
            other => panic!("expected PlanRequest, got {other:?}"),
        }
        // gate 上应有一个 pending waiter(registration 成功)。
        assert_eq!(
            gate.waiters().lock().len(),
            1,
            "dispatch_plan_request should register exactly one waiter"
        );
    }

    #[tokio::test]
    async fn dispatch_plan_ready_emits_plan_ready_with_real_markdown() {
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let cfg = test_cfg();
        let markdown = "## Plan\n1. read foo.rs\n2. edit bar.rs".to_string();

        dispatch_plan_ready(
            &gate,
            &session_subs,
            turn_tx,
            "sub-1".to_string(),
            cfg,
            markdown.clone(),
        )
        .await;

        let ev = turn_rx.recv().await.expect("PlanReady event missing");
        match ev.msg {
            EventMsg::PlanReady(ready) => {
                assert_eq!(
                    ready.markdown, markdown,
                    "PlanReady.markdown must be the caller-supplied markdown, not a placeholder"
                );
                assert!(
                    !ready.markdown.contains("placeholder"),
                    "placeholder string must not leak into PlanReady.markdown"
                );
            }
            other => panic!("expected PlanReady, got {other:?}"),
        }
        assert_eq!(
            gate.waiters().lock().len(),
            1,
            "dispatch_plan_ready should register exactly one waiter"
        );
    }

    #[tokio::test]
    async fn dispatch_plan_ready_blocking_blocks_until_plan_approval() {
        // 核心修复不变量:阻塞版 dispatch 在用户 PlanApproval 到达前必须
        // 挂起(与 spawn 版的关键差异)。这是 tool_exec 不再「未批准就推进」
        // 的保证 —— 若此测试失败,说明 agent 图会在 plan 审批前继续跑。
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let cfg = test_cfg();

        let gate_for_task = gate.clone();
        let subs_for_task = session_subs.clone();
        let task = tokio::spawn(async move {
            dispatch_plan_ready_blocking(
                &gate_for_task,
                &subs_for_task,
                turn_tx,
                "sub-blk".into(),
                cfg,
                "## Plan".into(),
            )
            .await
        });

        // 1. 必须先 emit PlanReady(且 register 了 waiter)。
        let ev = turn_rx.recv().await.expect("PlanReady event missing");
        let plan_id = match ev.msg {
            EventMsg::PlanReady(pr) => pr.plan_id,
            other => panic!("expected PlanReady, got {other:?}"),
        };

        // 2. 未投递决策,task 必须仍在阻塞。用 sleep + is_finished 非阻塞探测。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "blocking dispatch must not return before PlanApproval arrives"
        );

        // 3. 投递 AutoMode → task 解除阻塞,返回 Some(AutoMode)。
        assert!(gate.complete(plan_id, reflect_protocol::PlanApprovalChoice::AutoMode));
        let choice = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("dispatch must unblock after PlanApproval")
            .expect("task panicked");
        assert_eq!(choice, Some(reflect_protocol::PlanApprovalChoice::AutoMode));
    }

    #[tokio::test]
    async fn dispatch_plan_ready_blocking_returns_none_on_cancel() {
        // session Shutdown 触发 cfg.cancel.cancel() 时,阻塞中的 dispatch 必须
        // 退出并返回 None(不 flip mode),避免图节点在会话关闭时永久挂起。
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let cfg = test_cfg();
        let cancel = cfg.cancel.clone();

        let gate_for_task = gate.clone();
        let subs_for_task = session_subs.clone();
        let task = tokio::spawn(async move {
            dispatch_plan_ready_blocking(
                &gate_for_task,
                &subs_for_task,
                turn_tx,
                "sub-cancel".into(),
                cfg,
                "## Plan".into(),
            )
            .await
        });

        // 等 emit 完成确认 dispatch 已进入阻塞等待,再触发 cancel(避免 race)。
        turn_rx.recv().await.expect("PlanReady event missing");
        cancel.cancel();

        let choice = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cancel must unblock the dispatch")
            .expect("task panicked");
        assert_eq!(
            choice, None,
            "cancel must abort blocking dispatch with None"
        );
    }

    #[tokio::test]
    async fn dispatch_plan_request_with_dropped_turn_tx_does_not_panic() {
        // turn_tx 提前 drop 时,helper 应记录 warn 并安全返回(oneshot Receiver
        // drop 后 waiter task 静默退出),不应 panic / hang。
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let cfg = test_cfg();

        dispatch_plan_request(
            &gate,
            &session_subs,
            mpsc::channel::<Event>(8).0, // 立即丢弃 rx;tx 唯一引用在 helper 内
            "sub-2".to_string(),
            cfg,
            "task".to_string(),
        )
        .await;
        // waiter 已注册(注册在 send 失败之前),但 waiter task 会因 rx drop 退出。
        assert_eq!(gate.waiters().lock().len(), 1);
    }

    // ── v1.x:plan 事件落进 session JSONL 的端到端验证 ──────────────────
    //
    // CapturingRecorder 收集 record 调用,断言 dispatch_plan_ready /
    // dispatch_plan_request 真的把对应 RolloutRecord 写进了 recorder(而非
    // 只 emit 内存事件)。persist_rollout_best_effort 用 tokio::spawn,测试需
    // yield 一下让 spawn task 跑完。

    /// 测试用 recorder:把每次 record 调用收进 Vec,供断言。
    /// 包装 `parking_lot::Mutex<Vec<RolloutRecord>>`(与文件其余处同款锁)。
    #[derive(Debug, Default)]
    struct CapturingRecorder(parking_lot::Mutex<Vec<RolloutRecord>>);

    #[async_trait::async_trait]
    impl RolloutRecorder for CapturingRecorder {
        async fn record(&self, r: RolloutRecord) -> anyhow::Result<()> {
            self.0.lock().push(r);
            Ok(())
        }
        async fn replay(
            &self,
            _session_id: reflect_protocol::ThreadId,
        ) -> anyhow::Result<Vec<RolloutRecord>> {
            Ok(self.0.lock().clone())
        }
        async fn list_sessions(&self) -> anyhow::Result<Vec<reflect_protocol::SessionInfo>> {
            Ok(Vec::new())
        }
        async fn truncate_after(&self, _to_turn_id: Option<&TurnId>) -> anyhow::Result<usize> {
            Ok(0)
        }
    }

    /// 构造注入了 CapturingRecorder 的 AgentConfig。
    fn cfg_with_recorder(captured: Arc<CapturingRecorder>) -> AgentConfig {
        let mut m4 = crate::config::default_m4_deps("test");
        m4.recorder = Some(captured as Arc<dyn RolloutRecorder>);
        AgentConfig::new("anthropic/x", "/tmp").with_m4(m4)
    }

    /// 等待 tokio::spawn 的 best-effort 落盘 task 完成(yield 让出执行权)。
    async fn yield_for_spawns() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn dispatch_plan_ready_persists_plan_ready_record() {
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let captured = Arc::new(CapturingRecorder::default());
        let cfg = cfg_with_recorder(captured.clone());
        let markdown = "## Plan\n1. read foo\n2. edit bar".to_string();

        dispatch_plan_ready(
            &gate,
            &session_subs,
            turn_tx,
            "sub-1".to_string(),
            cfg,
            markdown.clone(),
        )
        .await;
        // 消费 emit 的事件,避免 channel 积压。
        let _ = turn_rx.recv().await;

        yield_for_spawns().await;

        let recs = captured.0.lock().clone();
        let plan_ready = recs
            .iter()
            .find_map(|r| match r {
                RolloutRecord::PlanReady { markdown, .. } => Some(markdown.clone()),
                _ => None,
            })
            .expect("PlanReady record should be persisted");
        assert_eq!(plan_ready, markdown, "persisted markdown must match input");
    }

    #[tokio::test]
    async fn dispatch_plan_request_persists_plan_request_record() {
        let gate = Arc::new(PlanApprovalGate::new());
        let session_subs: Arc<Mutex<Vec<mpsc::Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let (turn_tx, mut turn_rx) = mpsc::channel::<Event>(8);
        let captured = Arc::new(CapturingRecorder::default());
        let cfg = cfg_with_recorder(captured.clone());

        dispatch_plan_request(
            &gate,
            &session_subs,
            turn_tx,
            "sub-1".to_string(),
            cfg,
            "refactor auth".to_string(),
        )
        .await;
        let _ = turn_rx.recv().await;
        yield_for_spawns().await;

        let recs = captured.0.lock().clone();
        let task = recs
            .iter()
            .find_map(|r| match r {
                RolloutRecord::PlanRequest { task, .. } => Some(task.clone()),
                _ => None,
            })
            .expect("PlanRequest record should be persisted");
        assert_eq!(task, "refactor auth");
    }

    // ── persist_plan_markdown:plan 落盘为持久产物 ────────────────────────
    //
    // 验证:plan markdown 在 dispatch_plan_ready 时被写到
    // `<workspace>/.reflect/plan/<plan_id>.md`,成为可引用、可 `cat` 的文件。
    // 失败 best-effort(返回 None,不 panic)。

    #[test]
    fn persist_plan_markdown_writes_file_under_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = PlanId::new();
        let md = "## Plan\n1. read auth.rs\n2. edit token validation";

        let path = persist_plan_markdown(tmp.path(), pid, md);

        let path = path.expect("落盘成功应返回 Some(path)");
        assert!(
            path.starts_with(tmp.path().join(DEFAULT_PLAN_DIR)),
            "path 应在 <workspace>/.reflect/plan/ 下, got {}",
            path.display()
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(format!("{pid}.md").as_str()),
            "文件名应是 <plan_id>.md"
        );
        assert!(path.exists(), "文件应真实存在");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            md,
            "文件内容应等于传入的 markdown"
        );
    }

    #[test]
    fn persist_plan_markdown_creates_missing_dir() {
        // .reflect/plan/ 不存在时,create_dir_all 应自动建出。
        let tmp = tempfile::tempdir().unwrap();
        let plan_dir = tmp.path().join(DEFAULT_PLAN_DIR);
        assert!(!plan_dir.exists(), "前置:目录应不存在");

        let path = persist_plan_markdown(tmp.path(), PlanId::new(), "# plan");

        assert!(path.is_some(), "应返回 Some");
        assert!(plan_dir.exists(), "目录应被自动创建");
        assert!(plan_dir.is_dir());
    }

    #[test]
    fn persist_plan_markdown_returns_none_on_failure() {
        // 不可写路径(在一个文件下当 workspace)应 best-effort 失败:
        // 只返回 None,不 panic。
        let tmp = tempfile::tempdir().unwrap();
        let file_as_ws = tmp.path().join("not-a-dir");
        std::fs::write(&file_as_ws, b"x").unwrap(); // 这是个文件,不是目录

        let path = persist_plan_markdown(&file_as_ws, PlanId::new(), "# plan");

        assert!(path.is_none(), "不可写 workspace 应返回 None 而非 panic");
    }
}
