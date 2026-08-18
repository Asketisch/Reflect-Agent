//! `ApprovalGate` —— 单 turn 内,需要 `PermissionMode::Prompt` 的工具的审批路由。
//!
//! 流程(M6):
//! 1. `ToolExecutionQueue::execute_single` 检查 tool 的 `required_permission`。
//!    若为 `Prompt` 且用户未把该工具加入本 session 白名单,queue 调用
//!    [`ApprovalGate::ask_tool`]。
//! 2. `ask_tool` 生成新的 `request_id`,注册一个 `tokio::sync::oneshot`
//!    waiter,并在单 turn 的 event channel 上发送 `EventMsg::ApprovalRequest`。
//! 3. TUI(或任意客户端)处理该事件,并发送
//!    `Op::ToolApproval { id: request_id, decision }`。
//! 4. `submission_loop` 按 `sub_id` 找到对应 gate,调用
//!    [`ApprovalGate::complete`],完成 oneshot。
//! 5. `ask_tool` 返回 `ReviewDecision`,queue 据此行动。
//!
//! `tokio::select!` 集成 cancel:token 触发 cancel 时短路等待并返回
//! `ReviewDecision::Deny`。

mod gate;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tracing::warn;

use reflect_protocol::{AskUserAnswer, ReviewDecision};

pub use gate::ApprovalGate;

/// 待处理审批的 `request_id` → oneshot waiter 映射。
pub type ApprovalWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<ReviewDecision>>>>;

/// 待处理 ask-user-question 的 `request_id` → oneshot waiter,负责把
/// 结构化的 `AskUserAnswer` 送回 `AskUserQuestionTool::execute`。
///
/// 与 `ApprovalWaiters` 平行但**独立**(不混用同一个 map,避免 `ReviewDecision` /
/// `AskUserAnswer` 类型擦除带来的混乱)。由 `submission_loop` 持全局共享,
/// per-turn `ApprovalGate` 通过 `with_state(..., Some(qw.clone()))` 共享引用;
/// `Op::AskUserQuestionResponse` 由 `complete_ask_user_question` 全局函数路由。
pub type AskUserQuestionWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<AskUserAnswer>>>>;

/// 待处理 `ask_user` 的 `request_id` → oneshot waiter,负责把用户自由文本
/// 回执送到 `AskUserTool::execute`。
pub type AskUserInputWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;

/// v1.2 review P2:bug-2:`ask_user` 工具 prompt 字节上限。LLM 误发
/// 超过此长度的字符串在 `ApprovalGate::ask_user` 入口直接 `InvalidArgs`
/// 拒绝,避免 event channel 撑爆 + TUI 渲染无滚动卡死。
///
/// 16 KiB 远超实际问答 prompt(常见 1 KiB 以内),又能 OOM 之前熔断。
pub const MAX_ASK_USER_PROMPT_BYTES: usize = 16 * 1024;

pub fn complete_ask_user_input(
    waiters: &AskUserInputWaiters,
    request_id: &str,
    text: String,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(text).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "ask_user completion arrived but no waiter registered (cancelled?)"
            );
            false
        }
    }
}

/// 在共享 waiter map 上直接完成一个待处理的 ask-user-question。
/// 给 `submission_loop` 用 —— 这样它不需要为每个到来的 `Op::AskUserQuestionResponse`
/// 找到最初的那个 gate。request_id 是 uuid,在单个 session 内全局唯一。
pub fn complete_ask_user_question(
    waiters: &AskUserQuestionWaiters,
    request_id: &str,
    answers: AskUserAnswer,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(answers).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "ask_user_question completion arrived but no waiter registered (cancelled?)"
            );
            false
        }
    }
}

/// 在共享 waiter map 上直接完成一个待处理的审批。给 `submission_loop`
/// 用 —— 这样它不需要为每个到来的 `Op::ToolApproval` 找到最初的那个
/// gate。request_id 是 uuid,在单个 session 内全局唯一。
pub fn complete_approval(
    waiters: &ApprovalWaiters,
    request_id: &str,
    decision: ReviewDecision,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(decision).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "approval completion arrived but no waiter is registered (already cancelled?)"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests;
