//! `pricing` —— 按模型计费计算。
//!
//! 维护一张 USD / 1M token 的静态费率表，覆盖 Reflect v0 支持的
//! 提供方（Anthropic Claude、OpenAI GPT-4o / o3-mini）。
//! `price(model, usage)` 返回单次调用的 USD 成本；
//! `cumulative_cost_usd(model, history)` 对一个会话内所有 turn 求和。
//! 未识别的模型返回 `None`（对应 wire 事件中 `cost_usd` 字段序列化为缺失）——
//! 与其显示一个错误的数字，不如不显示。
//!
//! ## 感知缓存的计费（M8 P1a）
//!
//! Anthropic 与 OpenAI 都会对缓存命中的输入按折扣计费。四个分段及其计费方式：
//!
//! | 分段 | Anthropic 倍率 | OpenAI 倍率 |
//! |---------|------------------|---------------|
//! | `non_cached_input = input - cached - cache_write` | 1.0× | 1.0× |
//! | `cached`（cache_read） | 0.1× | 0.5×（auto-prompt-cache） |
//! | `cache_write`（cache_creation） | 1.25× | n/a（OpenAI 仅自动缓存） |
//! | `output` | 5.0× | 4.0×（gpt-4o）/ 4.0×（o3-mini） |
//!
//! 计费数据为 `const`（无网络依赖），便于测试 fixture 保持稳定。
//! 表内费率对标 Anthropic 2026-Q2 公开牌价与 OpenAI 2026-Q2 公开牌价；建议按季度刷新。
//!
//! ## 更新节奏
//!
//! 提供方调整公开牌价需要发版（代码改动 + 一个 M-x.y 版本）。
//! `cost_usd` 字段仅为参考信息；对账时务必以提供方 dashboard 为准。

use reflect_protocol::TokenUsage;

/// 每 1M token 的 USD 费率，按四个计费分段拆分。
/// `*_factor` 字段以 `input_price` 为基准；绝对价格推导为
/// `segment_price = input_price * factor`。这样表格更紧凑，
/// 同时让相对倍率一目了然。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// 每 1M 输入 token 的 USD 价格（非缓存分段的基准价）。
    pub input_price: f64,
    /// 相对 `input_price` 的输出倍率（Anthropic 5×，OpenAI 4×）。
    pub output_factor: f64,
    /// 缓存读取倍率（Anthropic 0.1×，OpenAI auto 0.5×）。
    pub cache_read_factor: f64,
    /// 缓存写入倍率（Anthropic 1.25×，OpenAI 不适用 → 0）。
    pub cache_write_factor: f64,
    /// 提供方暴露的完整模型 id（例如 `"claude-3-5-sonnet-latest"`、`"gpt-4o"`）。
    /// 查找大小写不敏感；表中存储规范的小写形式。
    pub canonical_id: &'static str,
}

/// 静态费率表。查找复杂度为 O(n)，表项约 7 条；若调用方走热路径请本地缓存。
///
/// 数值为 USD / 1M token。**已于 2026-Q2 刷新**；提供方发布新牌价时同步更新。
const PRICING_TABLE: &[ModelPricing] = &[
    // ── Anthropic Claude 4 系列（2026-Q2）──────────────────────────
    ModelPricing {
        canonical_id: "claude-opus-4-latest",
        input_price: 15.0,
        output_factor: 5.0,
        cache_read_factor: 0.1,
        cache_write_factor: 1.25,
    },
    ModelPricing {
        canonical_id: "claude-sonnet-4-latest",
        input_price: 3.0,
        output_factor: 5.0,
        cache_read_factor: 0.1,
        cache_write_factor: 1.25,
    },
    ModelPricing {
        canonical_id: "claude-3-5-sonnet-latest",
        input_price: 3.0,
        output_factor: 5.0,
        cache_read_factor: 0.1,
        cache_write_factor: 1.25,
    },
    ModelPricing {
        canonical_id: "claude-3-5-haiku-latest",
        input_price: 0.80,
        output_factor: 5.0,
        cache_read_factor: 0.1,
        cache_write_factor: 1.25,
    },
    // ── OpenAI GPT 系列（2026-Q2）──────────────────────────────────
    ModelPricing {
        canonical_id: "gpt-4o",
        input_price: 5.0,
        output_factor: 4.0,      // 4× → $20 / Mtok 输出
        cache_read_factor: 0.5,  // auto-prompt-cache 命中享 5 折
        cache_write_factor: 0.0, // OpenAI 无显式缓存写入档
    },
    ModelPricing {
        canonical_id: "gpt-4o-mini",
        input_price: 0.15,
        output_factor: 4.0,
        cache_read_factor: 0.5,
        cache_write_factor: 0.0,
    },
    ModelPricing {
        canonical_id: "o3-mini",
        input_price: 1.10,
        output_factor: 4.0,
        cache_read_factor: 0.5,
        cache_write_factor: 0.0,
    },
    // ── Ollama（本地，v0.3.1）───────────────────────────────────────
    // 本地推理不按 token 计费 —— `input_price: 0.0` 让 `price()` 返回
    // `Some(0.0)` 而不是 `None`，TUI 显示 "$0.00" 而不是 "—"，与
    // "本地 = 免费" 的用户心智一致。
    //
    // 用户用 `:7b` / `:latest` / `:instruct` 等 tag 时，canonical_id 精确
    // 匹配失败 → 走 `None` 兜底（保守更好，避免给未知的 Ollama tag 算 $0）。
    ModelPricing {
        canonical_id: "llama3.2",
        input_price: 0.0,
        output_factor: 1.0,
        cache_read_factor: 0.0,
        cache_write_factor: 0.0,
    },
    ModelPricing {
        canonical_id: "qwen2.5",
        input_price: 0.0,
        output_factor: 1.0,
        cache_read_factor: 0.0,
        cache_write_factor: 0.0,
    },
    ModelPricing {
        canonical_id: "llama3.1",
        input_price: 0.0,
        output_factor: 1.0,
        cache_read_factor: 0.0,
        cache_write_factor: 0.0,
    },
    ModelPricing {
        canonical_id: "mistral-nemo",
        input_price: 0.0,
        output_factor: 1.0,
        cache_read_factor: 0.0,
        cache_write_factor: 0.0,
    },
    ModelPricing {
        canonical_id: "gemma2",
        input_price: 0.0,
        output_factor: 1.0,
        cache_read_factor: 0.0,
        cache_write_factor: 0.0,
    },
];

/// 按 `model_id` 查找费率（大小写不敏感）。model id 匹配时不包含 `provider/` 前缀；
/// `reflect-core::graph::nodes` 在调用前已经剥离该前缀。
fn lookup(model_id: &str) -> Option<&'static ModelPricing> {
    let needle = model_id.to_ascii_lowercase();
    PRICING_TABLE
        .iter()
        .find(|p| p.canonical_id.eq_ignore_ascii_case(&needle))
}

/// 判断 `model_id` 是否在静态费率表中有条目（大小写不敏感，`canonical_id` 精确匹配 —— 与 [`price`] 规则一致）。
///
/// 用于 TUI 状态栏指标，区分「本地免费模型」与「未定价/未知模型」，
/// 无需为探测而调用 [`price`]。未知模型返回 `false`。
pub fn is_priced(model_id: &str) -> bool {
    lookup(model_id).is_some()
}

/// 计算指定 `model_id` 一次 `TokenUsage` 的 USD 成本。
///
/// 模型未识别时返回 `None`；调用方应将缺失显式呈现，而非默认回退为 0.0。
///
/// ## 公式
/// ```text
/// non_cached_input = max(0, input - cached - cache_write)
/// cost_usd = (non_cached_input * input_price
///           + cached * cache_read_factor * input_price
///           + cache_write * cache_write_factor * input_price
///           + output * output_factor * input_price) / 1_000_000
/// ```
///
/// 各字段含义与边界情形参见函数体与 [`TokenUsage`] 文档注释。
pub fn price(model_id: &str, usage: &TokenUsage) -> Option<f64> {
    let p = lookup(model_id)?;
    let u = &usage;
    // 饱和减法：防御 `(cached + cache_write) > input` 这种病态情形
    // （例如上游计数字段出现 bug）。
    let non_cached = u
        .input_tokens
        .saturating_sub(u.cached_tokens)
        .saturating_sub(u.cache_write_tokens);
    let per_mtok = (non_cached as f64)
        + (u.cached_tokens as f64) * p.cache_read_factor
        + (u.cache_write_tokens as f64) * p.cache_write_factor
        + (u.output_tokens as f64) * p.output_factor;
    Some((per_mtok * p.input_price) / 1_000_000.0)
}

/// 对一个会话的 `TokenUsage` 历史求和得到累计 USD 成本。
/// 历史中无法定价的条目会被静默丢弃（仅对可定价条目累计汇报成本）。
///
/// 空历史返回 `Some(0.0)`（调用方请求总额，返回 0 而非 None）；
/// 非空历史只要至少有一条成功定价即返回 `Some(sum)`；
/// 仅当历史中**所有**条目都不可定价时才返回 `None` —— 会话从未触达已知模型，
/// 此时无法给出有意义的数字。
pub fn cumulative_cost_usd(model_id: &str, history: &[TokenUsage]) -> Option<f64> {
    if history.is_empty() {
        return Some(0.0);
    }
    let mut total = 0.0;
    let mut priced_any = false;
    for u in history {
        if let Some(c) = price(model_id, u) {
            total += c;
            priced_any = true;
        }
    }
    if priced_any { Some(total) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_protocol::TokenUsage;

    fn empty_usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
        }
    }

    #[test]
    fn price_unknown_model_returns_none() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 150,
        };
        assert_eq!(price("gpt-9000-future", &u), None);
        assert_eq!(price("", &u), None);
    }

    #[test]
    fn price_zero_usage_returns_zero_for_known_model() {
        assert_eq!(price("claude-3-5-sonnet-latest", &empty_usage()), Some(0.0));
    }

    #[test]
    fn price_anthropic_sonnet_3_5_basic_no_cache() {
        // 算例：1M input @ $3/Mtok，1M output @ $15/Mtok → $18.00
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 2_000_000,
        };
        let c = price("claude-3-5-sonnet-latest", &u).unwrap();
        assert!((c - 18.0).abs() < 1e-9, "expected $18.00, got {c}");
    }

    #[test]
    fn price_anthropic_sonnet_3_5_with_cache_read_and_write() {
        // 1M input 合计：600k 非缓存 + 300k cache_read + 100k cache_write
        // 500k 输出 token
        // 非缓存段：600_000 / 1M * 3.0 = $1.80
        // Cache read 段：300_000 * 0.1 / 1M * 3.0 = $0.09
        // Cache write 段：100_000 * 1.25 / 1M * 3.0 = $0.375
        // Output 段：500_000 * 5 / 1M * 3.0 = $7.50
        // 合计：$9.765
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cached_tokens: 300_000,
            cache_write_tokens: 100_000,
            total_tokens: 1_500_000,
        };
        let c = price("claude-3-5-sonnet-latest", &u).unwrap();
        assert!((c - 9.765).abs() < 1e-6, "expected $9.765, got {c}");
    }

    #[test]
    fn price_openai_gpt_4o_no_cache() {
        // 算例：1M input @ $5/Mtok，1M output @ $20/Mtok → $25.00
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 2_000_000,
        };
        let c = price("gpt-4o", &u).unwrap();
        assert!((c - 25.0).abs() < 1e-9, "expected $25.00, got {c}");
    }

    #[test]
    fn price_openai_gpt_4o_with_auto_cache() {
        // 1M input：500k 非缓存 + 500k 缓存命中
        // 200k 输出 token
        // 非缓存段：500_000 * 5 / 1M = $2.50
        // Cache read 段：500_000 * 0.5 * 5 / 1M = $1.25
        // Output 段：200_000 * 4 * 5 / 1M = $4.00
        // 合计：$7.75
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 200_000,
            cached_tokens: 500_000,
            cache_write_tokens: 0,
            total_tokens: 1_200_000,
        };
        let c = price("gpt-4o", &u).unwrap();
        assert!((c - 7.75).abs() < 1e-6, "expected $7.75, got {c}");
    }

    #[test]
    fn price_is_case_insensitive() {
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 1_000_000,
        };
        assert!(price("Claude-3-5-Sonnet-Latest", &u).unwrap() > 0.0);
        assert!(price("GPT-4o", &u).unwrap() > 0.0);
    }

    #[test]
    fn price_saturates_against_underreported_input() {
        // 病态情形:cached + cache_write > input。本地 `non_cached_input` 应饱和到 0
        // (不出现负值),但 cache_read 和 cache_write 分段仍按各自倍率计费
        // —— 它们是真实计费段,无论上游 `input_tokens` 统计是否遗漏。
        //
        //   non_cached = max(0, 100 - 80 - 50) = 0
        //   cache_read = 80  * 0.1  = 8
        //   cache_write = 50 * 1.25 = 62.5
        //   output 段 = 50 * 5 = 250
        //   per_mtok = 320.5
        //   cost = 320.5 * 3.0 / 1M = 0.0009615
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 80,
            cache_write_tokens: 50, // 80 + 50 = 130 > 100
            total_tokens: 150,
        };
        let c = price("claude-3-5-sonnet-latest", &u).unwrap();
        assert!(c >= 0.0, "cost must not go negative, got {c}");
        assert!(
            (c - 0.0009615).abs() < 1e-9,
            "expected $0.0009615 (saturated non_cached + cache segments + output), got {c}"
        );
    }

    #[test]
    fn cumulative_zero_for_empty_history() {
        assert_eq!(
            cumulative_cost_usd("claude-3-5-sonnet-latest", &[]),
            Some(0.0)
        );
    }

    #[test]
    fn cumulative_sums_all_entries_when_model_known() {
        // 所有条目均针对同一已知模型定价。"丢弃未知"语义仅对整条历史中
        // model_id 本身未知的场景生效(见下方测试)。
        let u1 = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 1_000_000,
        };
        let u2 = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 1_000_000,
        };
        let u_big = TokenUsage {
            input_tokens: 999_999_999,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 999_999_999,
        };
        // $3 + $3 + $2999.999997 = $3005.999997
        let total = cumulative_cost_usd("claude-3-5-sonnet-latest", &[u1, u2, u_big]).unwrap();
        assert!(
            (total - 3005.999997).abs() < 1e-3,
            "expected ~$3005.999997, got {total}"
        );
    }

    #[test]
    fn cumulative_returns_none_when_all_unknown() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 100,
        };
        assert_eq!(
            cumulative_cost_usd("gpt-future-9000", &[u.clone(), u]),
            None
        );
    }

    #[test]
    fn pricing_table_lookup_finds_all_canonical_ids() {
        // 自检:表中每个条目都必须能查到自己。
        for p in PRICING_TABLE {
            let found = lookup(p.canonical_id).unwrap();
            assert_eq!(found.canonical_id, p.canonical_id);
        }
    }

    // ── Ollama 本地计费（v0.3.1）───────────────────────────────────

    #[test]
    fn ollama_models_price_zero() {
        // 本地推理 → Some(0.0) 而不是 None,TUI 显示 "$0.00" 而非 "—"。
        let u = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 1_500,
        };
        for m in ["llama3.2", "qwen2.5", "llama3.1", "mistral-nemo", "gemma2"] {
            assert_eq!(price(m, &u), Some(0.0), "model {m} should price 0.0");
        }
    }

    #[test]
    fn ollama_unknown_substring_returns_none() {
        // 用户用 `llama3.2:7b` / `qwen2.5:latest` 等带 tag 的名字 → 表里
        // 没有精确匹配 → None(TUI 显示 "—",保守更好)。
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 150,
        };
        assert_eq!(price("llama3.2:7b", &u), None);
        assert_eq!(price("qwen2.5:latest", &u), None);
        assert_eq!(price("mistral", &u), None); // 短名不在表里
    }
}
