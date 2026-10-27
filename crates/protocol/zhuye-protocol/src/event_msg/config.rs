//! 配置热重载载荷(+ SystemTime serde 适配器)。

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// M7: 配置文件热重载事件。`path` 是触发变更的文件;`sections_changed` 是
/// 受影响段名列表(例如 `["anthropic", "compact"]`),便于 UI 只在关键段变更
/// 时提示。`at` 用 `SystemTime` 而非 `Instant`,因为事件可能跨进程持久化。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigReloadedEvent {
    pub path: PathBuf,
    pub sections_changed: Vec<String>,
    #[serde(with = "systemtime_serde")]
    pub at: SystemTime,
}

/// v1.6.1:`Op::PromptCommand { action: List }` 的应答 —— 可用提示词层与
/// 当前生效层。TUI/GUI 的 `/prompt` overlay 据此渲染选择列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptListEvent {
    /// 可绑定的层文件名(不含 `.md`;内置厂商层 + 用户目录层,去重)。
    pub available: Vec<String>,
    /// 当前 model spec(如 `"anthropic/claude-sonnet-5-5"`)。
    pub current_spec: String,
    /// 当前 spec 实际命中的用户层文件名(诊断用,内嵌层不记录)。
    pub active_layers: Vec<String>,
}

/// v1.6.1:`Op::PromptCommand { action: Switch }` 的应答 —— 提示词库重扫
/// 完成。UI 据此提示「提示词已刷新」并展示新命中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSwitchedEvent {
    /// 触发重扫的动作名(`"reload"`;预留未来聚焦切换的层名)。
    pub action: String,
    /// 重扫后当前 spec 的生效用户层(让 UI 立即看到新命中)。
    pub active_layers: Vec<String>,
}

/// `SystemTime` 的 serde 适配 —— 序列化为 UNIX 秒数,跨进程持久化稳定。
pub(super) mod systemtime_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        dur.as_secs().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}
