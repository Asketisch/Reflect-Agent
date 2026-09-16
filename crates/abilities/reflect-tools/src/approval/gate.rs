//! [`ApprovalGate`] 结构体定义、`Debug` 实现与全部方法实现。
//!
//! 类型别名、free function(`complete_*`)与常量留在 [`super`] (`mod.rs`),
//! 此文件只承载 gate 自身的逻辑,通过 `use super::*` 拿到共享状态类型。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use reflect_protocol::{
    ApprovalKind, ApprovalRequestEvent, AskUserAnswer, AskUserInputEvent, Event, EventMsg,
    PermissionBubbleEvent, PermissionMode, Question, ReviewDecision, RiskLevel, ToolError,
    question::AskUserQuestionEvent,
};

use super::{
    ApprovalWaiters, AskUserInputWaiters, AskUserQuestionWaiters, MAX_ASK_USER_PROMPT_BYTES,
    complete_approval, complete_ask_user_question,
};

/// 单 turn 的审批门。由 `submission_loop` 持有,交给
/// `ToolExecutionQueue::execute_all_with_gate` 使用。
pub struct ApprovalGate {
    /// v1.5 R1:`ThreadSettingsOverrides.approval_policy = deny` 时置位 ——
    /// 本回合所有需要审批的工具一律直接拒绝(不弹 modal、不问 resolver)。
    /// gate 为回合级实例,天然限定作用域。
    pub(super) deny_all: std::sync::atomic::AtomicBool,
    pub(super) event_tx: mpsc::Sender<Event>,
    pub(super) sub_id: String,
    pub(super) waiters: ApprovalWaiters,
    /// 用户在整个会话生命周期内已批准的工具。
    pub(super) session_allow: Arc<Mutex<HashSet<String>>>,
    /// S5a:可选的 permission resolver,`ask_tool` 入口先查规则短路
    /// Allow / Deny。`None` 时退回原 modal-only 行为(向后兼容)。
    pub(super) permission_resolver: Option<Arc<dyn reflect_permissions::PermissionResolver>>,
    /// v1.1.0 P1 #14:per-gate waiter map for `ask_user_question` 工具的
    /// 结构化问答。每个 `ApprovalGate` 自带一份,`submission_loop` 通过
    /// `Arc<ApprovalGate>` 调 `complete_question` 路由 `Op::AskUserQuestionResponse`
    /// 回执。
    pub(super) question_waiters: AskUserQuestionWaiters,
    /// v1.1.0 P1 #15:`ask_user` 自由文本询问 waiter map。
    pub(super) user_input_waiters: AskUserInputWaiters,
    /// 会话级 `PermissionMode`(AcceptEdits / Bubble 短路用)。
    pub(super) session_permission_mode: Option<Arc<parking_lot::RwLock<PermissionMode>>>,
    /// P2 `yolo-classifier`:Auto 模式下的启发式审批分类器。
    /// `None`(默认)= Auto 模式下落到 modal(向后兼容);`Some` 时 Auto 模式
    /// 且 resolver 无规则、mode 未自动批准时,查分类器:Allow(高置信)→ 自动批准,
    /// Deny → 拒绝,Ask / 低置信 → modal。
    pub(super) yolo_classifier: Mutex<Option<Arc<dyn reflect_permissions::YoloClassifier>>>,
    /// YOLO 自动批准的置信度阈值(>= 此值才信任 Allow 建议)。默认 0.8。
    pub(super) yolo_threshold: Mutex<f32>,
}

// 手写 Debug —— `Arc<dyn PermissionResolver>` 没 derive Debug。
impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("sub_id", &self.sub_id)
            .field("waiters", &self.waiters)
            .field("session_allow", &self.session_allow)
            .field(
                "permission_resolver",
                &self
                    .permission_resolver
                    .as_ref()
                    .map(|_| "<dyn PermissionResolver>"),
            )
            .field("question_waiters", &self.question_waiters)
            .field("user_input_waiters", &self.user_input_waiters)
            .field(
                "session_permission_mode",
                &self
                    .session_permission_mode
                    .as_ref()
                    .map(|_| "<RwLock<PermissionMode>>"),
            )
            .field("yolo_classifier", &self.yolo_classifier.lock().is_some())
            .field("yolo_threshold", &*self.yolo_threshold.lock())
            .finish()
    }
}

impl ApprovalGate {
    pub fn new(event_tx: mpsc::Sender<Event>, sub_id: impl Into<String>) -> Self {
        Self::with_state(
            event_tx,
            sub_id,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            None,
            None,
            None,
            None,
        )
    }

    /// 构造一个共享"session 内已批准"集合的 gate,使兄弟 turn 共用(从而
    /// turn N 的 `ApproveForSession` 决策可以延续到 turn N+1)。Waiters
    /// 是该 gate 私有的。
    pub fn with_session_allow(
        event_tx: mpsc::Sender<Event>,
        sub_id: impl Into<String>,
        session_allow: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self::with_state(
            event_tx,
            sub_id,
            Arc::new(Mutex::new(HashMap::new())),
            session_allow,
            None,
            None,
            None,
            None,
        )
    }

    /// 构造一个共享 `waiters` 与 `session_allow` 的 gate。这让 `submission_loop`
    /// 能把传入的 `Op::ToolApproval` 路由到任一 gate 写入的同一个 waiter
    /// map(request_id 是 uuid,跨 turn 不会撞 id)。
    ///
    /// S5a:第 5 参数 `permission_resolver` 可选。`Some(r)` 时 `ask_tool`
    /// 入口先查 `r.resolve(tool)` 短路 Allow / Deny;`None` 时回退到
    /// 原 modal 行为。**不**修改 `ask_tool` 以外的现有路径语义。
    ///
    /// v1.1.0 P1 #14:第 6 参数 `question_waiters` 可选。`Some(qw)` 时 gate
    /// 把 `ask_user_question` 工具的 oneshot waiter 注册到共享 map(由
    /// `submission_loop` 持全局,`Op::AskUserQuestionResponse` 路由用);
    /// `None` 时 per-gate 新建一个独立 map(向后兼容 + 简单场景)。
    ///
    /// v1.1.0 P1 #15:第 7 参数 `user_input_waiters` —— `ask_user` 工具用。
    /// 第 8 参数 `session_permission_mode` —— AcceptEdits / Bubble 短路。
    #[allow(clippy::too_many_arguments)]
    pub fn with_state(
        event_tx: mpsc::Sender<Event>,
        sub_id: impl Into<String>,
        waiters: ApprovalWaiters,
        session_allow: Arc<Mutex<HashSet<String>>>,
        permission_resolver: Option<Arc<dyn reflect_permissions::PermissionResolver>>,
        question_waiters: Option<AskUserQuestionWaiters>,
        user_input_waiters: Option<AskUserInputWaiters>,
        session_permission_mode: Option<Arc<parking_lot::RwLock<PermissionMode>>>,
    ) -> Self {
        Self {
            deny_all: std::sync::atomic::AtomicBool::new(false),
            event_tx,
            sub_id: sub_id.into(),
            waiters,
            session_allow,
            permission_resolver,
            question_waiters: question_waiters
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new()))),
            user_input_waiters: user_input_waiters
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new()))),
            session_permission_mode,
            yolo_classifier: Mutex::new(None),
            yolo_threshold: Mutex::new(0.8),
        }
    }

    /// v1.5 R1:置位/解除回合级 deny-all。置位后,本回合所有走 gate 的
    /// 审批(`ask_tool` / `ask_hook`)一律直接 `Deny`,不弹 modal。
    /// 由 `submission_loop` 在 `approval_policy = deny` 时调用 —— gate
    /// 为回合级实例,作用域天然限定单回合。
    pub fn set_deny_all(&self, on: bool) {
        self.deny_all.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// P2 `yolo-classifier`:注入 Auto 模式启发式审批分类器 + 可选置信度阈值。
    /// `threshold` < 0 时维持当前值。
    pub fn set_yolo_classifier(
        &self,
        classifier: Option<Arc<dyn reflect_permissions::YoloClassifier>>,
        threshold: Option<f32>,
    ) {
        *self.yolo_classifier.lock() = classifier;
        if let Some(t) = threshold {
            if t >= 0.0 {
                *self.yolo_threshold.lock() = t;
            }
        }
    }

    pub fn sub_id(&self) -> &str {
        &self.sub_id
    }

    pub fn waiters(&self) -> &ApprovalWaiters {
        &self.waiters
    }

    /// v1.1.0 P1 #14:per-gate waiter map for `ask_user_question` 工具。
    pub fn question_waiters(&self) -> &AskUserQuestionWaiters {
        &self.question_waiters
    }

    /// v1.1.0 P1 #15:per-gate waiter map for `ask_user` 工具。
    pub fn user_input_waiters(&self) -> &AskUserInputWaiters {
        &self.user_input_waiters
    }

    pub fn session_allow_handle(&self) -> Arc<Mutex<HashSet<String>>> {
        self.session_allow.clone()
    }

    pub fn is_session_allowed(&self, tool_name: &str) -> bool {
        self.session_allow.lock().contains(tool_name)
    }

    pub fn allow_for_session(&self, tool_name: impl Into<String>) {
        self.session_allow.lock().insert(tool_name.into());
    }

    /// 完成一个待处理的审批请求。`submission_loop` 在收到 `Op::ToolApproval`
    /// 或 `Op::HookApproval` 时调用。返回 `true` 表示找到匹配的 waiter
    /// 并完成决策。
    pub fn complete(&self, request_id: &str, decision: ReviewDecision) -> bool {
        complete_approval(&self.waiters, request_id, decision)
    }

    /// v1.1.0 P1 #14:per-gate 包装的 `complete_ask_user_question`(per-gate
    /// `question_waiters` 模式)。`submission_loop` 默认走全局
    /// `complete_ask_user_question` 函数(共享 map),tool 端调试时可用
    /// 这个 method。返回 `true` 表示找到 waiter 并成功发送。
    pub fn complete_question(&self, request_id: &str, answers: AskUserAnswer) -> bool {
        complete_ask_user_question(&self.question_waiters, request_id, answers)
    }

    /// 向用户请求审批一次工具调用。emit `ApprovalRequest` 后等待 oneshot,
    /// 直到对应的 `Op::ToolApproval` 到达、cancel token 触发或 event channel
    /// 关闭。
    ///
    /// S5a:入口先查 `permission_resolver`(若挂载):
    /// - `RuleMatch::Allow` → 直接返回 `Approve`(**不**写 session_allow,
    ///   一次 tool call 的 allow 不该污染"session 持久"语义;
    ///   真正想"session 内总是允许"用 `/permissions allow <tool>` 持久规则)。
    /// - `RuleMatch::Deny` → 直接返回 `Deny { reason }`。
    /// - `RuleMatch::Ask` 或 `NoMatch` → 走正常 modal 流程(后者退化)。
    ///
    /// 短路优先于 session_allow 缓存 + 用户 modal 三段决策链 —— 显式规则
    /// 比"上次点了 Always Allow"更权威。
    pub async fn ask_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
        cancel: &CancellationToken,
    ) -> ReviewDecision {
        // 0. v1.5 R1:回合级 deny-all(`approval_policy = deny`)最优先。
        if self.deny_all.load(std::sync::atomic::Ordering::SeqCst) {
            return ReviewDecision::Deny {
                reason: "approval_policy = deny(本回合拒绝所有需审批的工具)".into(),
            };
        }
        // 1. S5a:resolver 短路。Ask / NoMatch / None → fall through。
        // 带 bash 命令上下文调 `resolve_with_context`,让 `shell_pattern`
        // 规则(如 `Bash: git *`)能命中 —— 否则该类规则静默失效。
        // Bash 工具命令在 args["cmd"](见 builtins/bash.rs schema)。
        let mut force_modal = false;
        if let Some(resolver) = &self.permission_resolver {
            let bash_cmd = args.get("cmd").and_then(|v| v.as_str());
            match resolver.resolve_with_context(tool_name, bash_cmd).await {
                reflect_permissions::RuleMatch::Allow => {
                    return ReviewDecision::Approve;
                }
                reflect_permissions::RuleMatch::Deny => {
                    return ReviewDecision::Deny {
                        reason: format!("denied by rule for {tool_name}"),
                    };
                }
                reflect_permissions::RuleMatch::Ask => {
                    force_modal = true;
                }
                reflect_permissions::RuleMatch::NoMatch => {}
            }
        }

        // 2. 会话 `PermissionMode` 短路(AcceptEdits / Bubble / Deny)。
        // 显式 `RuleMatch::Ask` 规则优先,不覆盖。
        //
        // v1.3 safety baseline(plan §五):即使会话模式通常会放行工具
        // (`Bubble` 一律放行;文件编辑类工具在 `AcceptEdits` 下放行),
        // `risk == High` 仍然 override,强制弹出人工审批 modal。
        // 这堵死了"Bubble 一刀切全部放行"和
        // "ApproveForSession 在 bash 上覆盖 Dangerous"两个后门:
        // 任何高危副作用(`sudo` / `rm -rf` / shell-installer 等)
        // 每次都必须显式人工确认。
        if !force_modal {
            if let Some(mode_ref) = &self.session_permission_mode {
                let mode = *mode_ref.read();
                if mode == PermissionMode::Deny {
                    return ReviewDecision::Deny {
                        reason: "session permission mode is deny".into(),
                    };
                }
                if risk != RiskLevel::High && mode.auto_approves_tool(tool_name) {
                    if mode == PermissionMode::Bubble {
                        self.emit_permission_bubble(tool_name, args, risk).await;
                    }
                    return ReviewDecision::Approve;
                }
            }
        }

        // 3. P2 `yolo-classifier`:Auto 模式下(resolver 无规则 + mode 未自动批准),
        // 查启发式分类器。Allow 且置信度 >= 阈值 → 自动批准(免 modal);
        // Deny → 拒绝;Ask / 低置信 → 继续 modal。无分类器时维持原 modal 行为。
        if !force_modal {
            if let Some(classifier) = self.yolo_classifier.lock().clone() {
                let args_str = serde_json::to_string(args).unwrap_or_default();
                let class = classifier.classify(tool_name, &args_str);
                let threshold = *self.yolo_threshold.lock();
                match class.suggestion {
                    reflect_permissions::YoloSuggestion::Allow if class.confidence >= threshold => {
                        return ReviewDecision::Approve;
                    }
                    reflect_permissions::YoloSuggestion::Deny => {
                        return ReviewDecision::Deny {
                            reason: format!("denied by yolo classifier: {}", class.reason),
                        };
                    }
                    // Allow 低置信 / Ask → 落到 modal。
                    _ => {}
                }
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ReviewDecision>();
        self.waiters.lock().insert(request_id.clone(), tx);

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::ApprovalRequest(ApprovalRequestEvent {
                request_id: request_id.clone(),
                kind: ApprovalKind::Tool {
                    tool_name: tool_name.to_string(),
                    args: args.clone(),
                },
                risk,
            }),
        );

        if self.event_tx.send(ev).await.is_err() {
            // Receiver 已 drop —— 没有消费者,无法拿到决策,自动拒绝。
            self.waiters.lock().remove(&request_id);
            return ReviewDecision::Deny {
                reason: "approval channel closed".into(),
            };
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.waiters.lock().remove(&request_id);
                ReviewDecision::Deny { reason: "cancelled".into() }
            }
            res = rx => {
                match res {
                    Ok(decision) => decision,
                    Err(_) => ReviewDecision::Deny {
                        reason: "approval channel dropped".into(),
                    },
                }
            }
        }
    }

    /// 向用户请求审批一次 hook 决策(`ask_tool` 的 `ApprovalKind::Hook` 版本)。
    /// M6 起 hook 引入 `HookDecision::Ask` 时会接通此路径。
    pub async fn ask_hook(
        &self,
        hook_name: &str,
        decision_preview: impl Into<String>,
        risk: RiskLevel,
        cancel: &CancellationToken,
    ) -> ReviewDecision {
        // v1.5 R1:deny-all 同样覆盖 hook 级审批。
        if self.deny_all.load(std::sync::atomic::Ordering::SeqCst) {
            return ReviewDecision::Deny {
                reason: "approval_policy = deny(本回合拒绝所有需审批的工具)".into(),
            };
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ReviewDecision>();
        self.waiters.lock().insert(request_id.clone(), tx);

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::ApprovalRequest(ApprovalRequestEvent {
                request_id: request_id.clone(),
                kind: ApprovalKind::Hook {
                    hook_name: hook_name.to_string(),
                    decision_preview: decision_preview.into(),
                },
                risk,
            }),
        );

        if self.event_tx.send(ev).await.is_err() {
            self.waiters.lock().remove(&request_id);
            return ReviewDecision::Deny {
                reason: "approval channel closed".into(),
            };
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.waiters.lock().remove(&request_id);
                ReviewDecision::Deny { reason: "cancelled".into() }
            }
            res = rx => res.unwrap_or(ReviewDecision::Deny {
                reason: "approval channel dropped".into(),
            }),
        }
    }

    /// v1.1.0 P1 #14:LLM 主动向用户发起 1-4 道结构化问题。emit
    /// `EventMsg::AskUserQuestion`,oneshot 等待 `Op::AskUserQuestionResponse`
    /// 回执或 cancel。
    ///
    /// # Errors
    ///
    /// - `ToolError::InvalidArgs`:`questions` 为空 / 超过 4 道 / 某道
    ///   `header` 超过 12 字符 / 某道 `options` 数量不在 [2,4] 范围。
    /// - `ToolError::Cancelled`:`cancel` token 在等待响应时触发。
    /// - `ToolError::Execution`:event channel 已关闭,无法 emit。
    ///
    /// # Flow
    ///
    /// 1. 校验 `questions`(数量、每道 `header` / `options` 边界)。
    /// 2. 分配 `request_id` (uuid),oneshot channel 写入 `question_waiters`。
    /// 3. emit `EventMsg::AskUserQuestion` 给 TUI / headless 客户端。
    /// 4. `tokio::select!` 等回执 / cancel / channel 关闭。
    /// 5. 返回 `AskUserAnswer`(可能为空 — 用户按 Esc 取消)。
    pub async fn ask_question(
        &self,
        questions: Vec<Question>,
        cancel: &CancellationToken,
    ) -> Result<AskUserAnswer, ToolError> {
        // 1. 校验 questions 数量。
        if questions.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "ask_user_question: 'questions' must not be empty".into(),
            });
        }
        if questions.len() > reflect_protocol::question::MAX_QUESTIONS {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "ask_user_question: too many questions ({} > {})",
                    questions.len(),
                    reflect_protocol::question::MAX_QUESTIONS
                ),
            });
        }
        // 2. 校验每道 question(把 `Question::new` 的 Result 转 ToolError)。
        for (i, q) in questions.iter().enumerate() {
            if q.options.len() < reflect_protocol::question::MIN_OPTIONS
                || q.options.len() > reflect_protocol::question::MAX_OPTIONS
            {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: question[{i}] options.len() = {} (must be in [{}, {}])",
                        q.options.len(),
                        reflect_protocol::question::MIN_OPTIONS,
                        reflect_protocol::question::MAX_OPTIONS
                    ),
                });
            }
            if q.header.chars().count() > reflect_protocol::question::MAX_HEADER_CHARS {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: question[{i}] header too long ({} > {})",
                        q.header.chars().count(),
                        reflect_protocol::question::MAX_HEADER_CHARS
                    ),
                });
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<AskUserAnswer>();
        self.question_waiters.lock().insert(request_id.clone(), tx);

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::AskUserQuestion(AskUserQuestionEvent::new(request_id.clone(), questions)),
        );

        if self.event_tx.send(ev).await.is_err() {
            self.question_waiters.lock().remove(&request_id);
            return Err(ToolError::Execution(
                "ask_user_question: event channel closed".into(),
            ));
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.question_waiters.lock().remove(&request_id);
                Err(ToolError::Cancelled)
            }
            res = rx => match res {
                Ok(answers) => Ok(answers),
                Err(_) => Err(ToolError::Execution(
                    "ask_user_question: waiter dropped before response".into(),
                )),
            }
        }
    }

    /// Bubble 模式:emit 非阻塞通知,不等待用户决策。
    async fn emit_permission_bubble(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) {
        let preview = args
            .as_object()
            .map(|o| {
                o.iter()
                    .take(3)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty());
        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::PermissionBubble(PermissionBubbleEvent {
                tool_name: tool_name.to_string(),
                args_preview: preview,
                risk,
            }),
        );
        let _ = self.event_tx.send(ev).await;
    }

    /// v1.1.0 P1 #15:LLM 向用户发起自由文本询问。emit `AskUserInput`,
    /// oneshot 等待 `Op::AskUserInputResponse` 或 cancel。
    ///
    /// # 超时(v1.2 review P1:bug-1)
    ///
    /// `timeout_secs` 控制最大等待秒数;`0` 表示永不超时(由 cancel token
    /// 或 event channel 关闭兜底,适合 `request_human_input` 这类持久化
    /// 等待场景)。`> 0` 时 `tokio::time::sleep` 与 cancel / rx 一起
    /// `tokio::select!`;超时到达返回
    /// `ToolError::Execution("ask_user: timed out after N seconds")`,
    /// 同步清理 `user_input_waiters` 防止泄漏。
    ///
    /// # 权限短路(v1.2 review P1:bug-2)
    ///
    /// `tool_name` 用于 resolver 查询(`ask_user` / `request_human_input`)。
    /// - resolver `Deny` rule → `InvalidArgs`(显式规则赢)。
    /// - session `PermissionMode::Deny` → `InvalidArgs`。
    /// - session `PermissionMode::Bubble` → `InvalidArgs`(无法对自由文本
    ///   做"自动同意",保守退化为 Deny)。
    /// - resolver `Allow` rule → **不**短路(自由文本没有合法默认值,
    ///   必须由用户显式输入;该规则对 `ask_user` 无效)。
    ///
    /// # Prompt 长度(v1.2 review P2:bug-2)
    ///
    /// `prompt` 字节数超 [`MAX_ASK_USER_PROMPT_BYTES`] 直接拒;防止
    /// LLM 误发 100 MB 字符串挤爆 event channel + TUI 渲染。
    pub async fn ask_user(
        &self,
        tool_name: &str,
        prompt: impl Into<String>,
        cancel: &CancellationToken,
        timeout_secs: u64,
    ) -> Result<String, ToolError> {
        // v1.2 P0:委托给带选项的核心实现(默认非敏感)。
        self.ask_user_opts(tool_name, prompt, cancel, timeout_secs, false, None)
            .await
    }

    /// v1.2 P0:带 `secret` / `placeholder` 选项的 `ask_user`。
    /// `secret = true` 时 TUI 把输入渲染为 `•` 掩码(密码 / API key);
    /// `placeholder` 提供空输入时的占位提示。回执 `text` 仍为明文。
    pub async fn ask_user_opts(
        &self,
        tool_name: &str,
        prompt: impl Into<String>,
        cancel: &CancellationToken,
        timeout_secs: u64,
        secret: bool,
        placeholder: Option<&str>,
    ) -> Result<String, ToolError> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "ask_user: 'prompt' must not be empty".into(),
            });
        }
        if prompt.len() > MAX_ASK_USER_PROMPT_BYTES {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "ask_user: 'prompt' too long ({} > {} bytes)",
                    prompt.len(),
                    MAX_ASK_USER_PROMPT_BYTES
                ),
            });
        }

        // v1.2 review P1:bug-2:权限短路(镜像 ask_tool:294-378)。
        // 1. resolver 短路:`Deny` 立即拒绝;`Allow` 不短路(自由文本无默认)。
        if let Some(resolver) = &self.permission_resolver {
            if let reflect_permissions::RuleMatch::Deny = resolver.resolve(tool_name).await {
                return Err(ToolError::InvalidArgs {
                    message: format!("ask_user: denied by rule for {tool_name}"),
                });
            }
        }
        // 2. session `PermissionMode` 短路:`Deny` / `Bubble` 拒绝。
        if let Some(mode_ref) = &self.session_permission_mode {
            let mode = *mode_ref.read();
            if mode == PermissionMode::Deny {
                return Err(ToolError::InvalidArgs {
                    message: "ask_user: session permission mode is deny".into(),
                });
            }
            if mode == PermissionMode::Bubble {
                return Err(ToolError::InvalidArgs {
                    message:
                        "ask_user: session permission mode is bubble (no auto-answer for free text)"
                            .into(),
                });
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<String>();
        self.user_input_waiters
            .lock()
            .insert(request_id.clone(), tx);

        // v1.2 P0:透传 secret / placeholder 到事件(向后兼容:旧 consumer 默认 false/None)。
        let mut event = AskUserInputEvent::new(request_id.clone(), prompt);
        if secret {
            event = event.with_secret(true);
        }
        if let Some(ph) = placeholder {
            event = event.with_placeholder(ph);
        }
        let ev = Event::new(self.sub_id.clone(), EventMsg::AskUserInput(event));

        if self.event_tx.send(ev).await.is_err() {
            self.user_input_waiters.lock().remove(&request_id);
            return Err(ToolError::Execution(
                "ask_user: event channel closed".into(),
            ));
        }

        // v1.2 review P1:bug-1:把"rx 收到用户响应"包装成 Result,避免
        // `match res { ... }` 在 `tokio::select!` handler 里的复杂模式让
        // macro parser 困惑。
        let rx_result: Result<String, String> = if timeout_secs == 0 {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("cancelled".into()),
                recv = rx => recv.map_err(|_| "dropped".into()),
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("cancelled".into()),
                recv = rx => recv.map_err(|_| "dropped".into()),
                _sleep = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                    Err(format!("timeout:{timeout_secs}"))
                }
            }
        };

        match rx_result {
            Ok(text) => Ok(text),
            Err(reason) if reason == "cancelled" => {
                self.user_input_waiters.lock().remove(&request_id);
                Err(ToolError::Cancelled)
            }
            Err(reason) if reason == "dropped" => Err(ToolError::Execution(
                "ask_user: waiter dropped before response".into(),
            )),
            Err(reason) if reason.starts_with("timeout:") => {
                self.user_input_waiters.lock().remove(&request_id);
                Err(ToolError::Execution(format!(
                    "ask_user: timed out after {timeout_secs} seconds"
                )))
            }
            Err(other) => Err(ToolError::Execution(format!(
                "ask_user: unexpected wait outcome: {other}"
            ))),
        }
    }
}
