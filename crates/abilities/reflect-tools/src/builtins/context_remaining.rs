//! `get_context_remaining` — v1.2 P1-12 报告会话级 token 用量与剩余预算。
//!
//! 让 agent 在长会话中主动感知「上下文 / 预算还剩多少」,从而在接近
//! 上限时主动收尾 / 触发压缩 / 拒绝新任务,而不是被 `TokenBudgetExceeded`
//! 硬截断。只读、并发安全、`Auto` 权限(不打扰用户)。
//!
//! 数据来源(均通过 `ToolContext` 共享句柄,与 `model_call` 实时同步):
//! - `session_usage` —— 会话级累计 token(input/output/cached/cache_write/total)
//! - `token_budget` —— `[token_budget].session_total_tokens` / env 上限
//! - `context_window_size` —— 引擎在 `SessionConfigured` 时写入的模型窗口

use async_trait::async_trait;
use serde_json::Value;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// `get_context_remaining` — 报告会话 token 用量与剩余上下文 / 预算。
pub struct GetContextRemainingTool;

#[async_trait]
impl Tool for GetContextRemainingTool {
    fn name(&self) -> &str {
        "get_context_remaining"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.get_context_remaining")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    // 只读,无需打扰用户 —— 与 echo / glob / grep 同级。
    fn required_permission(&self) -> reflect_protocol::PermissionMode {
        reflect_protocol::PermissionMode::Auto
    }

    async fn execute(&self, ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let usage = ctx.session_usage.read().clone();
        let window = *ctx.context_window_size.read();
        let budget = *ctx.token_budget.read();

        // 上下文窗口剩余(本轮可再发的近似量):窗口 - 已累计总量。
        // 这是保守估计 —— 实际单轮可用还受本轮已发 token 影响,但累计量
        // 是会话级最关键的「还能撑多久」信号。
        let context_remaining = window.map(|w| w.saturating_sub(usage.total_tokens));

        // 预算剩余(硬上限):budget - 已累计总量。`None` = 无预算上限。
        let budget_remaining = budget.map(|b| b.saturating_sub(usage.total_tokens as u64));

        let pct_of_window = match window {
            Some(w) if w > 0 => Some((usage.total_tokens as f64 / w as f64) * 100.0),
            _ => None,
        };
        let pct_of_budget = match budget {
            Some(b) if b > 0 => Some((usage.total_tokens as f64 / b as f64) * 100.0),
            _ => None,
        };

        // 文本内容:给人 / LLM 一眼可读的摘要。
        let mut lines = Vec::new();
        lines.push(format!(
            "Session usage: {} input / {} output / {} cached / {} total tokens",
            usage.input_tokens, usage.output_tokens, usage.cached_tokens, usage.total_tokens
        ));
        match window {
            Some(w) => {
                let rem = context_remaining.unwrap_or(0);
                let pct = pct_of_window
                    .map(|p| format!(" ({p:.1}% used)"))
                    .unwrap_or_default();
                lines.push(format!("Context window: {w} tokens — {rem} remaining{pct}"));
            }
            None => {
                lines.push("Context window: unknown (model not in context-window table)".into())
            }
        }
        match budget {
            Some(b) => {
                let rem = budget_remaining.unwrap_or(0);
                let pct = pct_of_budget
                    .map(|p| format!(" ({p:.1}% used)"))
                    .unwrap_or_default();
                lines.push(format!("Token budget: {b} tokens — {rem} remaining{pct}"));
            }
            None => lines.push("Token budget: none (only max_iterations bounds the loop)".into()),
        }
        let content = reflect_protocol::ContentBlock::Text {
            text: lines.join("\n"),
        };

        Ok(ToolOutput {
            content: vec![content],
            is_error: false,
            metadata: serde_json::json!({
                "session_usage": {
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cached_tokens": usage.cached_tokens,
                    "cache_write_tokens": usage.cache_write_tokens,
                    "total_tokens": usage.total_tokens,
                },
                "context_window_size": window,
                "context_remaining": context_remaining,
                "token_budget": budget,
                "budget_remaining": budget_remaining,
                "pct_of_context_window_used": pct_of_window,
                "pct_of_budget_used": pct_of_budget,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::sync::Arc;

    /// 辅助:构造一个注入了用量 / 窗口 / 预算的 `ToolContext`。
    fn ctx_with(
        usage: reflect_protocol::TokenUsage,
        window: Option<u32>,
        budget: Option<u64>,
    ) -> ToolContext {
        ToolContext {
            session_usage: Arc::new(RwLock::new(usage)),
            context_window_size: Arc::new(RwLock::new(window)),
            token_budget: Arc::new(RwLock::new(budget)),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn reports_usage_window_and_budget() {
        let usage = reflect_protocol::TokenUsage {
            input_tokens: 10_000,
            output_tokens: 2_000,
            cached_tokens: 5_000,
            cache_write_tokens: 0,
            total_tokens: 12_000,
        };
        let ctx = ctx_with(usage, Some(200_000), Some(50_000));
        let out = GetContextRemainingTool
            .execute(ctx, serde_json::json!({}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // 文本含三个段落。
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text block"),
        };
        assert!(
            text.contains("12_000")
                || text.contains("12000")
                || text.contains("12,000")
                || text.contains("12 000 total")
        );
        assert!(text.contains("200"));
        assert!(text.contains("50"));
        // 元数据字段齐。
        assert_eq!(out.metadata["session_usage"]["total_tokens"], 12_000);
        assert_eq!(out.metadata["context_window_size"], 200_000);
        assert_eq!(out.metadata["context_remaining"], 188_000);
        assert_eq!(out.metadata["token_budget"], 50_000);
        assert_eq!(out.metadata["budget_remaining"], 38_000);
        // 百分比计算正确。
        let pct_w = out.metadata["pct_of_context_window_used"].as_f64().unwrap();
        assert!((pct_w - 6.0).abs() < 0.01); // 12000/200000 = 6%
        let pct_b = out.metadata["pct_of_budget_used"].as_f64().unwrap();
        assert!((pct_b - 24.0).abs() < 0.01); // 12000/50000 = 24%
    }

    #[tokio::test]
    async fn unknown_model_window_is_null() {
        let ctx = ctx_with(reflect_protocol::TokenUsage::default(), None, None);
        let out = GetContextRemainingTool
            .execute(ctx, serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.metadata["context_window_size"].is_null());
        assert!(out.metadata["context_remaining"].is_null());
        assert!(out.metadata["token_budget"].is_null());
        assert!(out.metadata["budget_remaining"].is_null());
    }

    #[tokio::test]
    async fn budget_exhausted_reports_zero_remaining() {
        let usage = reflect_protocol::TokenUsage {
            total_tokens: 60_000,
            ..Default::default()
        };
        let ctx = ctx_with(usage, Some(200_000), Some(50_000));
        let out = GetContextRemainingTool
            .execute(ctx, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.metadata["budget_remaining"], 0);
        // 上下文窗口仍按 200000 - 60000 算。
        assert_eq!(out.metadata["context_remaining"], 140_000);
    }

    #[tokio::test]
    async fn is_read_only_and_auto() {
        let t = GetContextRemainingTool;
        assert!(t.is_concurrency_safe());
        assert_eq!(
            t.required_permission(),
            reflect_protocol::PermissionMode::Auto
        );
    }
}
