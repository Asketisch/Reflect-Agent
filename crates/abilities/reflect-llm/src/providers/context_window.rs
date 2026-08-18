//! `context_window` — 每模型上下文窗口大小(token)静态表。
//!
//! 仿 [`pricing.rs`] 模式:一张 `const` 表 + 一个 `context_window_for` 查询函数。
//! 供 `SessionConfiguredEvent.context_window_size`(引擎报告)+ TUI 上下文用量条
//! (分子 / 分母)复用,避免与 LLM 层漂移。
//!
//! ## 归一化
//!
//! 输入 `model_spec` 可能带 `provider/` 前缀、ollama tag(`:7b`/`:latest`)、
//! 大小写差异。归一化三步:
//! 1. `rsplit_once('/')` 去 provider 前缀(`anthropic/claude-...` → `claude-...`)。
//! 2. `split(':')` 去 ollama tag(`llama3.2:7b` → `llama3.2`)。
//! 3. 小写化,`starts_with` 匹配版本后缀。
//!
//! 未知模型返回 `None`(TUI 优雅省略上下文条,比显示错数字更好)。

/// 静态模型 → 上下文窗口(token)表。覆盖 Reflect 支持的主线模型族。
/// **更新节奏**:跟 provider 官方文档同步;新模型上线时补条目。
const CONTEXT_WINDOW_TABLE: &[(&str, u32)] = &[
    // ── Anthropic Claude(200k 标准) ──────────────────────────────────
    ("claude-opus-4", 200_000),
    ("claude-sonnet-4", 200_000),
    ("claude-3-5-sonnet", 200_000),
    ("claude-3-5-haiku", 200_000),
    ("claude-3-opus", 200_000),
    ("claude-3-haiku", 200_000),
    // ── OpenAI(128k 标准;o3-mini 200k) ─────────────────────────────
    ("gpt-4o", 128_000),
    ("gpt-4o-mini", 128_000),
    ("gpt-4-turbo", 128_000),
    ("gpt-4", 128_000),
    ("o3-mini", 200_000),
    // ── Ollama 本地模型(128k 通用;以主流 config 为准) ────────────────
    ("llama3.3", 128_000),
    ("llama3.2", 128_000),
    ("llama3.1", 128_000),
    ("qwen2.5", 128_000),
    ("mistral-nemo", 128_000),
    ("gemma2", 8_000),
    // ── MiniMax(M3/M4 系列官方 1M 上下文,MSA 稀疏注意力) ──────────────
    // `minimax-m` 前缀覆盖 M1/M3/M4 等;`abab` 是老命名(abab6.5 等)。
    ("minimax-m", 1_000_000),
    ("abab", 245_000),
];

/// 查 `model_spec` 的上下文窗口大小（token）。归一化 provider 前缀 /
/// ollama tag / 大小写后用 `starts_with` 匹配版本后缀。
///
/// 返回 `None` 表示未知模型（TUI 此时省略上下文条）。
///
/// # 示例
///
/// 已知模型（带 / 不带 `provider/` 前缀、带 ollama tag）均返回命中值，
/// 未知模型返回 `None`：
///
/// ```
/// use reflect_llm::context_window_for;
/// assert_eq!(context_window_for("anthropic/claude-sonnet-4-latest"), Some(200_000));
/// assert_eq!(context_window_for("openai/gpt-4o"), Some(128_000));
/// assert_eq!(context_window_for("llama3.2:7b"), Some(128_000));
/// assert_eq!(context_window_for("unknown-model"), None);
/// ```
pub fn context_window_for(model_spec: &str) -> Option<u32> {
    // 1. 去 provider 前缀(`rsplit_once('/')` 取最后一段,无 '/' 时整串)。
    let no_provider = model_spec
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_spec);
    // 2. 去 ollama tag(`:7b` / `:latest` / `:instruct`)。
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    // 3. 小写化匹配。
    let needle = no_tag.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    CONTEXT_WINDOW_TABLE
        .iter()
        .find(|(prefix, _)| needle.starts_with(prefix))
        .map(|(_, size)| *size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_provider_prefix() {
        assert_eq!(
            context_window_for("anthropic/claude-sonnet-4-latest"),
            Some(200_000)
        );
        assert_eq!(context_window_for("openai/gpt-4o"), Some(128_000));
        assert_eq!(context_window_for("ollama/llama3.2"), Some(128_000));
    }

    #[test]
    fn strips_ollama_tag() {
        assert_eq!(context_window_for("llama3.2:7b"), Some(128_000));
        assert_eq!(context_window_for("qwen2.5:latest"), Some(128_000));
        assert_eq!(context_window_for("gpt-4o:instruct"), Some(128_000));
        assert_eq!(context_window_for("ollama/llama3.1:8b"), Some(128_000));
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(context_window_for("Claude-Sonnet-4-Latest"), Some(200_000));
        assert_eq!(context_window_for("GPT-4O"), Some(128_000));
        assert_eq!(context_window_for("O3-MINI"), Some(200_000));
    }

    #[test]
    fn matches_version_suffixes() {
        // `starts_with` 让版本后缀(-latest / -20240229 等)都能命中。
        assert_eq!(
            context_window_for("claude-3-5-sonnet-20241022"),
            Some(200_000)
        );
        assert_eq!(context_window_for("gpt-4o-2024-08-06"), Some(128_000));
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(context_window_for("future-model-9000"), None);
        assert_eq!(context_window_for(""), None);
        assert_eq!(context_window_for("/"), None);
    }

    #[test]
    fn bare_model_without_prefix() {
        // 无 provider 前缀也能查。
        assert_eq!(context_window_for("claude-opus-4-latest"), Some(200_000));
        assert_eq!(context_window_for("o3-mini"), Some(200_000));
    }

    #[test]
    fn minimax_m3_1m_context() {
        // MiniMax M3/M4 官方 1M 上下文(带/不带 provider 前缀、大小写)。
        assert_eq!(context_window_for("MiniMax-M3"), Some(1_000_000));
        assert_eq!(context_window_for("minimax/MiniMax-M3"), Some(1_000_000));
        assert_eq!(context_window_for("minimax-m4"), Some(1_000_000));
        // 老命名 abab 系列(abab6.5 = 245k)。
        assert_eq!(context_window_for("abab6.5-chat"), Some(245_000));
    }
}
