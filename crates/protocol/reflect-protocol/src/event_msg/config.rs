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
