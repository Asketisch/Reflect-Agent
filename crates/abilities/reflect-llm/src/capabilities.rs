//! Provider / model 能力标志。

/// 模型 / provider 组合对外声明的静态能力。
///
/// `reflect-core` 据此决定是否注入 cache breakpoint、是否启用
/// extended thinking、是否发送 system blocks 等。
#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    /// 是否支持 tool / function calling。
    pub tool_use: bool,
    /// 是否支持 prompt caching(Anthropic `cache_control`)。
    pub prompt_caching: bool,
    /// 是否支持 extended thinking(Anthropic thinking 块)。
    pub extended_thinking: bool,
    /// 是否接受图片输入。
    pub vision: bool,
    /// 是否支持严格 JSON 模式输出。
    pub json_mode: bool,
    /// 是否接受多块 system prompt。
    pub system_blocks: bool,
}

/// 强类型 provider 标识 —— `ModelClient::provider_kind()` 的返回类型。
///
/// v1.0.0-rc2 引入:替代 `name() -> &str` 的弱字符串比较,让 plugin /
/// TUI / tracing 可在不 unwrap 字符串的前提下做 provider 级分支。
///
/// `Custom` 是默认值 —— plugin / test stub / 未来扩展走这个变体;
/// 3 个内置 client 各自 override 为 `Anthropic` / `OpenAI` / `Ollama`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Ollama,
    /// 默认值 —— plugin / test stub / 未来扩展走这个。
    #[default]
    Custom,
}
