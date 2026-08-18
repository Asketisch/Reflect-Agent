//! `install_from_marketplace` 的聚合报告类型。
//!
//! Phase F 新增 —— 装一个 plugin 连带它的 dep closure,过程中可能部分成功 / 部分失败。

use crate::errors::PluginError;
use crate::identifier::PluginId;
use crate::state::InstallationEntry;

/// `install_from_marketplace` 的聚合结果 —— Phase F 新增。
///
/// 装一个 plugin 连带它的 dep closure,过程中可能部分成功 / 部分失败;
/// `installed` 列出成功装上的(id + entry),`failed` 列出失败的(target + error)。
#[derive(Debug, Default, Clone)]
pub struct InstallReport {
    /// 成功装上的 plugin(id + entry),按装入顺序。
    pub installed: Vec<(PluginId, InstallationEntry)>,
    /// 失败的 plugin(原始 dep 字面量或 `<name>@<mkt>` + error)。
    pub failed: Vec<PluginInstallFailure>,
}

/// 单条安装失败 —— `target` 走用户友好格式(`<name>@<mkt>` 或原始 dep 字面量)。
#[derive(Debug, Clone)]
pub struct PluginInstallFailure {
    pub target: String,
    pub error: PluginError,
}

impl InstallReport {
    pub fn is_success(&self) -> bool {
        self.failed.is_empty()
    }
    pub fn summary(&self) -> String {
        format!(
            "{} installed, {} failed",
            self.installed.len(),
            self.failed.len()
        )
    }
}
