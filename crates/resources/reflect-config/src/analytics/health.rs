//! 分析遥测 exporter 健康状态。
//!
//! 用 `parking_lot::Mutex<Health>` 持有全局最近一次 init 结果。`init_exporter`
//! 在 `Ok` 时设 `Healthy`,在端点缺失 / exporter 构建失败 / provider 构建失败
//! 时设 `Degraded(reason)`。`Doctor` 读取此状态生成状态行。

use parking_lot::Mutex;
use std::sync::OnceLock;

/// 当前 exporter 健康状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// exporter 启动成功。
    Healthy,
    /// 未启用遥测(`enabled=false` / `None`),不算错误。
    Disabled,
    /// 启用但初始化失败,记一个 reason。
    Degraded(String),
}

impl Health {
    /// 简短标签,用于 doctor 状态行。
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Disabled => "disabled",
            Health::Degraded(_) => "degraded",
        }
    }
}

impl std::fmt::Display for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Health::Healthy | Health::Disabled => f.write_str(self.as_str()),
            Health::Degraded(reason) => write!(f, "degraded:{reason}"),
        }
    }
}

static CURRENT: OnceLock<Mutex<Health>> = OnceLock::new();

fn cell() -> &'static Mutex<Health> {
    CURRENT.get_or_init(|| Mutex::new(Health::Disabled))
}

/// 读取当前健康状态(用于 doctor / 测试)。
pub fn current() -> Health {
    cell().lock().clone()
}

/// 标记 exporter 启动成功。
pub fn set_healthy() {
    *cell().lock() = Health::Healthy;
}

/// 标记 exporter 跳过 / 失败。
pub fn set_degraded(reason: &str) {
    *cell().lock() = Health::Degraded(reason.to_string());
}

/// 标记未启用(默认值)。
pub fn set_disabled() {
    *cell().lock() = Health::Disabled;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_round_trip() {
        assert_eq!(Health::Healthy.as_str(), "healthy");
        assert_eq!(Health::Disabled.as_str(), "disabled");
        assert_eq!(Health::Degraded("x".into()).as_str(), "degraded");
    }

    #[test]
    fn display_includes_reason_for_degraded() {
        assert_eq!(Health::Healthy.to_string(), "healthy");
        assert_eq!(
            Health::Degraded("endpoint".into()).to_string(),
            "degraded:endpoint"
        );
    }
}
