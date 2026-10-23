//! 会话统计读模型(v1.x)。
//!
//! [`SessionStats`] / [`ModelUsage`] 是 rollout JSONL 的**读取侧聚合结果**,
//! 由 `reflect-rollout::index::session_stats` / `all_model_usage` 计算。
//! 放在 protocol 层是为了让 rollout / telemetry / CLI / SDK 门面共享同一份
//! 契约 —— 历史上 token 聚合曾有 CLI、rollout index、reflect-py 三份独立
//! 复刻,统计类型统一后消费方不再各自定义。
//!
//! 与 [`crate::recorder::SessionInfo`] 的关系:`SessionInfo` 是 `session ls`
//! 列表用的轻量条目(只读首行 + 行扫描聚合);`SessionStats` 是全量扫描后的
//! 完整统计(按模型拆分 + 工具调用/压缩次数 + 缓存分段),供 `session show`
//! 与 SDK 查询使用。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::item::ThreadId;

/// 单个模型的累计用量。
///
/// `model` 为完整 spec(如 `"openai/gpt-4o"`);旧 JSONL 的 `TokenCount`
/// 无 `model` 字段时,聚合方回退到会话级 `SessionMeta.model`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    /// 该模型命中的 provider 名(如 `"anthropic"`)。同一模型可能经多个
    /// provider 路由,取首次出现的值;`None` = 旧记录无该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 该模型的 LLM 调用次数(= 归属该模型的 `TokenCount` 记录数)。
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `cache_read` 分段,是 `input_tokens` 的子集,不重复计入 total。
    pub cached_tokens: u64,
    /// `cache_creation` 分段,同样是 `input_tokens` 的子集。
    pub cache_write_tokens: u64,
    /// `input + output`(cached / cache_write 是 input 子集,刻意不加)。
    pub total_tokens: u64,
    /// 累计 USD cost。`None` = 该模型无任何带价的调用(不在 pricing 表)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 单会话统计:总量 + 按模型拆分 + 工具调用/压缩次数。
///
/// 由 `reflect-rollout::index::session_stats` 全量扫描会话 JSONL 计算。
/// 旧 JSONL(无 `TokenCount.model` 字段)的 per-model 用量全部归入
/// `SessionMeta.model` 一个条目,保证历史会话也有模型级视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub session_id: ThreadId,
    /// 会话级默认模型(`SessionMeta.model`)。
    pub model: String,
    pub started_at: DateTime<Utc>,
    /// 非空行数 - 1(减去首行 SessionMeta),与 `SessionInfo` 同口径。
    pub message_count: usize,
    /// 工具调用次数:Assistant 消息 content 中 `ContentBlock::ToolUse`
    /// 块的总数。
    pub tool_calls: u64,
    /// 压缩次数:`Compaction` 记录数。
    pub compactions: u64,
    /// v1.x:各次压缩前上下文 token 估算之和(`Compaction.before_tokens`)。
    /// 旧 JSONL 无该字段 → 0。
    pub compaction_before_tokens: u64,
    /// v1.x:各次压缩后上下文 token 估算之和(`Compaction.after_tokens`)。
    pub compaction_after_tokens: u64,
    /// v1.x:记忆检索注入次数(`MemoryAccess` Read 记录数)。
    pub memory_reads: u64,
    /// v1.x:记忆固化写入次数(`MemoryAccess` Write 记录数)。
    pub memory_writes: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 会话累计 `cache_read` 分段(input 子集)。
    pub cached_tokens: u64,
    /// 会话累计 `cache_creation` 分段(input 子集)。
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    /// 会话累计 USD cost。`None` = 无任何带价调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// 按模型拆分的用量,按 `total_tokens` 降序(用量最大的模型在前)。
    pub per_model: Vec<ModelUsage>,
}

impl SessionStats {
    /// 缓存命中率:`cached_tokens / input_tokens`。无 input 时返回 `None`。
    /// 仅 Anthropic 等支持 prompt caching 的 provider 有非零 cached;
    /// 其余 provider 恒 0,命中率即 0。
    pub fn cache_hit_rate(&self) -> Option<f64> {
        if self.input_tokens == 0 {
            return None;
        }
        Some(self.cached_tokens as f64 / self.input_tokens as f64)
    }

    /// v1.x:压缩累计节省的 token(before 之和 - after 之和)。
    /// 无压缩记录或旧 JSONL(无 token 字段)→ 0。
    pub fn compaction_tokens_saved(&self) -> u64 {
        self.compaction_before_tokens
            .saturating_sub(self.compaction_after_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_rate_none_when_no_input() {
        let stats = SessionStats {
            session_id: ThreadId::new(),
            model: "openai/gpt-4o".into(),
            started_at: Utc::now(),
            message_count: 0,
            tool_calls: 0,
            compactions: 0,
            compaction_before_tokens: 0,
            compaction_after_tokens: 0,
            memory_reads: 0,
            memory_writes: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
            per_model: Vec::new(),
        };
        assert_eq!(stats.cache_hit_rate(), None);
    }

    #[test]
    fn cache_hit_rate_computes_ratio() {
        let mut stats = SessionStats {
            session_id: ThreadId::new(),
            model: "anthropic/claude".into(),
            started_at: Utc::now(),
            message_count: 0,
            tool_calls: 0,
            compactions: 0,
            compaction_before_tokens: 0,
            compaction_after_tokens: 0,
            memory_reads: 0,
            memory_writes: 0,
            input_tokens: 1000,
            output_tokens: 0,
            cached_tokens: 250,
            cache_write_tokens: 0,
            total_tokens: 1000,
            cost_usd: None,
            per_model: Vec::new(),
        };
        assert_eq!(stats.cache_hit_rate(), Some(0.25));
        stats.cached_tokens = 0;
        assert_eq!(stats.cache_hit_rate(), Some(0.0));
    }

    #[test]
    fn serde_roundtrip_with_optional_fields_absent() {
        // 旧格式:cost_usd / provider 缺省时应能反序列化(向后兼容)。
        let json = r#"{"model":"openai/gpt-4o","calls":2,"input_tokens":10,
            "output_tokens":5,"cached_tokens":0,"cache_write_tokens":0,
            "total_tokens":15}"#;
        let usage: ModelUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.model, "openai/gpt-4o");
        assert_eq!(usage.provider, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.calls, 2);
    }

    #[test]
    fn compaction_tokens_saved_subtracts_after_from_before() {
        let mut stats = SessionStats {
            session_id: ThreadId::new(),
            model: "openai/gpt-4o".into(),
            started_at: Utc::now(),
            message_count: 0,
            tool_calls: 0,
            compactions: 2,
            compaction_before_tokens: 30_000,
            compaction_after_tokens: 8_000,
            memory_reads: 0,
            memory_writes: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
            per_model: Vec::new(),
        };
        assert_eq!(stats.compaction_tokens_saved(), 22_000);
        // 无压缩 → 0。
        stats.compaction_before_tokens = 0;
        stats.compaction_after_tokens = 0;
        assert_eq!(stats.compaction_tokens_saved(), 0);
    }
}
