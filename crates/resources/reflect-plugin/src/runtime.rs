//! 运行时插件装配 —— 从 `reflect-exec` 下沉而来的启动挂载与 reload 同步。
//!
//! 这里只依赖本 crate 与各能力 registry(tools/hooks/mcp/skills/subagent),
//! 因此 exec / serve / 门面 Builder / Python 绑定可以共用同一套装配入口:
//! - [`bootstrap_plugins`]:启动期构造 [`PluginRuntime`] 并同步 enabled 列表;
//! - [`reload_plugins`]:config `[plugins]` 段变更时 diff 同步挂/卸。

use std::collections::HashSet;
use std::sync::Arc;

use reflect_hooks::HookEngine;
use reflect_mcp::McpConnectionManager;
use reflect_skills::SkillsCatalog;
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;

use crate::commands_registry::CommandRegistry;
use crate::identifier::PluginId;
use crate::loader::{LoaderRegistries, register, scan, unregister};
use crate::manifest::PluginManifest;
use crate::state::PluginScope;
use crate::{PluginManager, default_plugins_root};

/// 运行时插件状态:manager + 共享 registry + 当前 enabled 集合 +
/// 可选 event sink(批次二十四 #5:emit `PluginLoaded` 给 JSONL / TUI)。
pub struct PluginRuntime {
    manager: PluginManager,
    registries: LoaderRegistries,
    enabled: HashSet<String>,
    /// 批次二十四(#5):`reflect exec` 的 JSONL event sink;每次
    /// `enable_one` 成功后 emit `PluginLoaded`。`None` 时 silent(headless
    /// 重载或无 reload_tx 场景)。TUI 不走这条路径(它用
    /// `App::populate_plugins_from_enabled` 直填 RenderState)。
    event_tx: Option<tokio::sync::mpsc::Sender<reflect_protocol::Event>>,
}

impl PluginRuntime {
    /// 构造 `PluginRuntime`;`HOME` 未设或 plugins 目录不存在时返回 `None`。
    pub fn try_new(
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookEngine>,
        mcp: Arc<McpConnectionManager>,
        skills: Arc<SkillsCatalog>,
        factory: Arc<SubAgentFactory>,
    ) -> Option<Self> {
        let root = default_plugins_root()?;
        let manager = PluginManager::load(&root).ok()?;
        let registries = LoaderRegistries::new(tools, hooks, mcp, skills, factory);
        Some(Self {
            manager,
            registries,
            enabled: HashSet::new(),
            event_tx: None,
        })
    }

    /// 按 `config.plugins.enabled_plugins` 同步挂载/卸载。
    pub async fn sync_enabled(&mut self, enabled_list: &[String]) {
        let new_set: HashSet<String> = enabled_list.iter().cloned().collect();

        for id in self.enabled.difference(&new_set) {
            if let Ok(pid) = PluginId::parse_user_input(id)
                && let Err(e) = unregister(&self.registries, &pid).await
            {
                tracing::warn!(plugin = %id, error = %e, "plugin unregister on reload failed");
            }
        }

        for id in new_set.difference(&self.enabled) {
            if let Err(e) = self.enable_one(id).await {
                tracing::warn!(plugin = %id, error = %e, "plugin enable failed");
            }
        }

        self.enabled = new_set;
    }

    /// 只读访问插件命令注册表 —— 用户输入 `/cmd args` 展开用。
    pub fn commands(&self) -> Arc<CommandRegistry> {
        Arc::clone(&self.registries.commands)
    }

    async fn enable_one(&self, id_str: &str) -> Result<(), String> {
        let id = PluginId::parse_user_input(id_str).map_err(|e| e.to_string())?;
        let entry = self
            .manager
            .lookup_entry(&id)
            .ok_or_else(|| format!("plugin not installed: {id_str}"))?;
        let manifest_path = entry.install_path.join("plugin.toml");
        let manifest = PluginManifest::from_path(&manifest_path).map_err(|e| e.to_string())?;
        let (loaded, scan_errs) = scan(&id, &manifest, &entry.install_path);
        for e in &scan_errs {
            tracing::warn!(
                plugin = %id_str,
                capability = %e.capability,
                error = %e.message,
                "plugin capability scan warning"
            );
        }
        // 批次二十四(#5):在 register 前捕获计数(register 消费 loaded)。
        let skill_count = loaded.skills.len();
        let command_count = loaded.commands.len();
        let scope_str = match entry.scope {
            PluginScope::Managed => "managed",
            PluginScope::User => "user",
            PluginScope::Project => "project",
            PluginScope::Local => "local",
        };
        let version = entry.version.clone();
        if let Err(errs) = register(&self.registries, &loaded).await {
            let msg = errs
                .iter()
                .map(|e| format!("{}:{}: {}", e.capability, e.name, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(msg);
        }
        tracing::info!(plugin = %id_str, "plugin capabilities registered");
        // 批次二十四(#5):emit `PluginLoaded` 让 JSONL drainer / headless
        // 消费者看到 caps 计数。失败静默(reload_tx 关闭 = 下游已退出)。
        if let Some(tx) = &self.event_tx {
            let msg =
                reflect_protocol::EventMsg::PluginLoaded(reflect_protocol::PluginLoadedEvent {
                    plugin: id_str.to_string(),
                    scope: scope_str.to_string(),
                    version,
                    skill_count,
                    command_count,
                });
            let _ = tx
                .send(reflect_protocol::Event::new(
                    reflect_protocol::EVENT_ID_NONE,
                    msg,
                ))
                .await;
        }
        Ok(())
    }
}

/// 共享 `Arc<tokio::sync::Mutex<Option<PluginRuntime>>>` 供 reload task 异步更新。
pub type SharedPluginRuntime = Arc<tokio::sync::Mutex<Option<PluginRuntime>>>;

/// 构造一个"无插件"的空 runtime 句柄 —— 测试 / 不启用插件的调用方
/// 用它满足参数占位,`expand_plugin_command` 对其直通。
pub fn empty_plugin_runtime() -> SharedPluginRuntime {
    Arc::new(tokio::sync::Mutex::new(None))
}

/// 启动期:构造 runtime 并同步 enabled 列表。`event_tx` 为 `Some` 时,
/// 每个成功挂载的插件会 emit `PluginLoaded`(批次二十四 #5)。
pub async fn bootstrap_plugins(
    tools: Arc<ToolRegistry>,
    hooks: Arc<HookEngine>,
    mcp: Arc<McpConnectionManager>,
    skills: Arc<SkillsCatalog>,
    factory: Arc<SubAgentFactory>,
    enabled_list: &[String],
    event_tx: Option<tokio::sync::mpsc::Sender<reflect_protocol::Event>>,
) -> SharedPluginRuntime {
    let shared: SharedPluginRuntime = Arc::new(tokio::sync::Mutex::new(None));
    let Some(mut rt) = PluginRuntime::try_new(tools, hooks, mcp, skills, factory) else {
        tracing::debug!("plugin runtime skipped (no HOME or plugins root)");
        return shared;
    };
    if let Some(tx) = event_tx {
        rt.event_tx = Some(tx);
    }
    rt.sync_enabled(enabled_list).await;
    *shared.lock().await = Some(rt);
    shared
}

/// reload 时 plugins 段变更:diff enabled 列表并同步。
pub async fn reload_plugins(shared: &SharedPluginRuntime, enabled_list: &[String]) {
    let mut guard = shared.lock().await;
    if let Some(rt) = guard.as_mut() {
        rt.sync_enabled(enabled_list).await;
    }
}
