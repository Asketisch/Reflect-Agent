//! 插件系统的错误类型。
//!
//! 所有上层调用点(`PluginManager`、loader、CLI、slash 命令)只暴露
//! `PluginError`;底层 IO / 解析 / 校验失败都收敛到这一个枚举,便于在
//! TUI / CLI / `ConfigReloaded.plugins_errors` 里聚合。

use std::path::PathBuf;
use thiserror::Error;

/// 插件相关错误的统一入口。
#[derive(Debug, Error)]
pub enum PluginError {
    /// Manifest 文件缺失、IO 失败、解析失败。
    #[error("manifest 读取失败: {path}: {source}")]
    ManifestIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("manifest 解析失败: {path}: {message}")]
    ManifestParse { path: PathBuf, message: String },

    /// Manifest 字段不符合 schema(命名正则、必填缺失、版本号格式等)。
    #[error("manifest 校验失败: {0}")]
    Validation(String),

    /// `PluginId` 字符串不合法(命名正则或保留名)。
    #[error("plugin id 不合法: {0}")]
    InvalidPluginId(String),

    /// Marketplace 名字不合法。
    #[error("marketplace 名不合法: {0}")]
    InvalidMarketplaceName(String),

    /// 解析 `dependencies` 时找不到声明的 marketplace。
    #[error("依赖 {dep} 在 {marketplace} 内不可见: 跨 marketplace 依赖被禁用")]
    DependencyCrossMarketplace { dep: String, marketplace: String },

    /// 解析 `dependencies` 时检测到环。
    #[error("依赖环: {cycle:?}")]
    DependencyCycle { cycle: Vec<String> },

    #[error("依赖未满足: {0}")]
    MissingDependency(String),

    /// 用户输入的 plugin 名字拼写或 marketplace 不匹配已装列表。
    #[error("插件未安装: {0}")]
    NotInstalled(String),

    #[error("插件已安装: {0}")]
    AlreadyInstalled(String),

    /// Managed scope plugin 试图被 disable / uninstall。
    #[error("managed 插件不可更改: {0}")]
    ManagedLocked(String),

    /// 安装目录已存在且被占用。
    #[error("安装目录冲突: {0}")]
    InstallDirConflict(PathBuf),

    /// Git clone / fetch 失败。
    #[error("git 拉取失败: {0}")]
    Git(String),

    /// HTTP 拉取失败。
    #[error("HTTP 拉取失败: {0}")]
    Http(String),

    /// Marketplace fetch 失败(Git / File / Directory / Github / Url 通用)。
    ///
    /// `kind` 是触发 fetch 的 `MarketplaceSource` 序列化形态,
    /// `stderr` 是底层命令输出或原因(便于用户排错)。
    #[error("marketplace fetch 失败: {kind}: {stderr}")]
    MarketplaceFetch { kind: String, stderr: String },

    /// Marketplace manifest 在 fetch 后仍找不到(目录布局不对、文件缺失)。
    #[error("marketplace manifest 未找到: {0}")]
    MarketplaceManifestNotFound(String),

    /// SHA-256 校验和与 manifest 不一致。
    #[error("checksum 不匹配: 期望 {expected}, 实际 {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// 持久化文件 IO / 解析失败。
    #[error("状态文件 IO 失败: {path}: {source}")]
    StateIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("状态文件解析失败: {0}")]
    StateParse(String),

    /// `enabled_plugins` 与 `installed_plugins.json` 不一致。
    #[error("配置漂移: enabled_plugins 含 {enabled},但 installed_plugins.json 不存在")]
    Drift { enabled: String },

    /// capability loader 把 plugin 的某类能力挂到主程序时失败(单个不阻断整体)。
    #[error("加载能力失败: capability={capability}, plugin={plugin}: {message}")]
    CapabilityLoad {
        plugin: String,
        capability: String,
        message: String,
    },
}

/// `Result<T>` 的便捷别名。
pub type Result<T> = std::result::Result<T, PluginError>;

// ── 手动 Clone ────────────────────────────────────────────────────────
//
// `std::io::Error` 没有实现 `Clone`,而 `ManifestIo` / `StateIo` 变体持有
// `std::io::Error` 作 `source` —— derive `Clone` 不可行。手动实现时用
// `io::Error::new(kind, msg)` 重建,保留 `ErrorKind` + 消息(对 `Display` 足够)。

impl Clone for PluginError {
    fn clone(&self) -> Self {
        match self {
            PluginError::ManifestIo { path, source } => PluginError::ManifestIo {
                path: path.clone(),
                source: std::io::Error::new(source.kind(), source.to_string()),
            },
            PluginError::ManifestParse { path, message } => PluginError::ManifestParse {
                path: path.clone(),
                message: message.clone(),
            },
            PluginError::Validation(s) => PluginError::Validation(s.clone()),
            PluginError::InvalidPluginId(s) => PluginError::InvalidPluginId(s.clone()),
            PluginError::InvalidMarketplaceName(s) => {
                PluginError::InvalidMarketplaceName(s.clone())
            }
            PluginError::DependencyCrossMarketplace { dep, marketplace } => {
                PluginError::DependencyCrossMarketplace {
                    dep: dep.clone(),
                    marketplace: marketplace.clone(),
                }
            }
            PluginError::DependencyCycle { cycle } => PluginError::DependencyCycle {
                cycle: cycle.clone(),
            },
            PluginError::MissingDependency(s) => PluginError::MissingDependency(s.clone()),
            PluginError::NotInstalled(s) => PluginError::NotInstalled(s.clone()),
            PluginError::AlreadyInstalled(s) => PluginError::AlreadyInstalled(s.clone()),
            PluginError::ManagedLocked(s) => PluginError::ManagedLocked(s.clone()),
            PluginError::InstallDirConflict(p) => PluginError::InstallDirConflict(p.clone()),
            PluginError::Git(s) => PluginError::Git(s.clone()),
            PluginError::Http(s) => PluginError::Http(s.clone()),
            PluginError::MarketplaceFetch { kind, stderr } => PluginError::MarketplaceFetch {
                kind: kind.clone(),
                stderr: stderr.clone(),
            },
            PluginError::MarketplaceManifestNotFound(s) => {
                PluginError::MarketplaceManifestNotFound(s.clone())
            }
            PluginError::ChecksumMismatch { expected, actual } => PluginError::ChecksumMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            },
            PluginError::StateIo { path, source } => PluginError::StateIo {
                path: path.clone(),
                source: std::io::Error::new(source.kind(), source.to_string()),
            },
            PluginError::StateParse(s) => PluginError::StateParse(s.clone()),
            PluginError::Drift { enabled } => PluginError::Drift {
                enabled: enabled.clone(),
            },
            PluginError::CapabilityLoad {
                plugin,
                capability,
                message,
            } => PluginError::CapabilityLoad {
                plugin: plugin.clone(),
                capability: capability.clone(),
                message: message.clone(),
            },
        }
    }
}
