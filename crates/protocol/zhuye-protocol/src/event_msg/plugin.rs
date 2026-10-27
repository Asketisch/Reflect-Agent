//! 插件 + 配额载荷。

use serde::{Deserialize, Serialize};

/// 批次二十四(#5):一个插件被 `bootstrap_plugins` 成功扫描 + 注册。
/// 镜像 `McpServerStartedEvent` 的结构 + producer 路径。`plugin` 是
/// `PluginId` 字符串(与 `/plugin` overlay 的 key 一致);`scope` 是
/// `user` / `project` / `builtin`(来自 `InstallationEntry.scope`);
/// `version` 来自 manifest;`skill_count` / `command_count` 来自
/// `LoadedPlugin.skills.len()` / `commands.len()`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginLoadedEvent {
    pub plugin: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub skill_count: usize,
    #[serde(default)]
    pub command_count: usize,
}

/// v1.x 功能 7:`EventMsg::QuotaExhausted` 的 payload。某 credential 的
/// token plan 配额耗尽时由 `model_call` emit,TUI 据此显示
/// 「plan A 配额耗尽,已切换到 plan B」提示。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaExhaustedEvent {
    /// provider 名(如 `"anthropic"`)。
    pub provider: String,
    /// 耗尽的 credential label(如 `"plan-a"`)。
    pub label: String,
    /// 本窗口已用 token。
    #[serde(default)]
    pub used_tokens: u64,
    /// 窗口配额上限。
    #[serde(default)]
    pub max_tokens: u64,
    /// cooldown 剩余秒数(窗口重置时间)。
    #[serde(default)]
    pub window_ends_secs: u64,
}
