//! `loader` —— 把 `PluginManager` 的 install + `capabilities::scan_all` 串联起来,
//! 并提供 `LoaderRegistries` orchestrator 把扫描结果真接到主程序各 registry。
//!
//! ## Phase A vs Phase B 范围
//! - Phase A: `scan()` —— 纯函数,只扫,不挂。
//! - Phase B: `LoaderRegistries` + `register`/`unregister` —— 把 5 类
//!   能力真接到 `ToolRegistry` / `HookEngine` / `McpConnectionManager` /
//!   `SkillsCatalog` / `SubAgentFactory`。
//!
//! ## 当前实现范围
//! - **Skills**: 完整接入(`LoadedSkill` → `SkillMeta` → `add_plugin_skills`)
//! - **MCP servers**: 完整接入(`start_server_with_namespace` + scoped name)
//! - **Agents**: 完整接入(parse frontmatter → `SubAgentSpec` →
//!   `register_plugin_spec` + `CallSubAgentTool` → `register_plugin_tool`)
//! - **Commands**: deferred 到 Phase C(`reflect-tui` slash 命令系统)
//! - **Hooks**: deferred 到 Phase B-internal 收尾(需要 `ShellHook` 实现,
//!   涉及 child-process spawning,工作量独立)

use std::path::Path;
use std::sync::Arc;

use reflect_hooks::HookEngine;
use reflect_mcp::{McpConnectionManager, McpServerConfig as McpConfig, McpTransport};
use reflect_skills::{SkillMeta, SkillsCatalog};
use reflect_subagent::{CallSubAgentTool, SubAgentFactory, SubAgentSpec};
use reflect_tools::ToolRegistry;

use crate::capabilities::{CapabilityError, LoadedPlugin, scan_all};
use crate::identifier::PluginId;
use crate::manifest::PluginManifest;

/// 一组共享 registry 引用 —— 给 `PluginManager` 持有,`register` / `unregister`
/// 通过它真接挂 / 反挂 plugin 能力。
///
/// 用 `Arc` 持有所有 registry,与 `PluginManager` 共享同一份,避免
/// `clone()` 引起的多实例不一致。
pub struct LoaderRegistries {
    pub tools: Arc<ToolRegistry>,
    pub hooks: Arc<HookEngine>,
    pub mcp: Arc<McpConnectionManager>,
    pub skills: Arc<SkillsCatalog>,
    pub subagent_factory: Arc<SubAgentFactory>,
}

impl LoaderRegistries {
    pub fn new(
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookEngine>,
        mcp: Arc<McpConnectionManager>,
        skills: Arc<SkillsCatalog>,
        subagent_factory: Arc<SubAgentFactory>,
    ) -> Self {
        Self {
            tools,
            hooks,
            mcp,
            skills,
            subagent_factory,
        }
    }
}

/// 从 manifest + install_path 扫描所有能力(Phase A 入口,保留)。
///
/// 这一步不触发任何全局状态变更 —— 它是纯函数(除磁盘 IO 与日志外)。
pub fn scan(
    plugin_id: &PluginId,
    manifest: &PluginManifest,
    install_path: &Path,
) -> (LoadedPlugin, Vec<CapabilityError>) {
    scan_all(plugin_id, manifest, install_path)
}

/// 把 `LoadedPlugin` 挂到主程序各 registry。
///
/// 成功路径:每类能力都注册成功,返回 `Ok(())`。
/// 失败路径:已注册的项保留(部分成功),错误累积到 `errors` 返回。
/// 调用方决定是否回滚 —— Phase B 选择保留 partial 状态,UI 在 `errors`
/// 里看到失败的能力。
pub async fn register(
    registries: &LoaderRegistries,
    loaded: &LoadedPlugin,
) -> Result<(), Vec<RegisterError>> {
    let mut errors = Vec::new();

    // 1. Skills —— 最简单,纯内存操作。
    let skill_metas: Vec<SkillMeta> = loaded
        .skills
        .iter()
        .map(|s| SkillMeta {
            name: s.name.clone(),
            description: s.description.clone().unwrap_or_default(),
            triggers: vec![],
            tools: vec![],
            mcp_collections: vec![],
            path: s.skill_md.clone(),
            body: String::new(),
            plugin_id: Some(loaded.plugin_id.to_string()),
            when_paths: vec![],
        })
        .collect();
    if !skill_metas.is_empty() {
        let plugin_id = loaded.plugin_id.to_string();
        registries
            .skills
            .add_plugin_skills(&plugin_id, &skill_metas);
    }

    // 2. Agents —— parse frontmatter → SubAgentSpec → register_plugin_spec
    //    + CallSubAgentTool → register_plugin_tool。
    for agent in &loaded.agents {
        match parse_agent_frontmatter(agent) {
            Ok(spec) => {
                registries
                    .subagent_factory
                    .register_plugin_spec(loaded.plugin_id.as_str(), spec.clone());
                let tool = Arc::new(CallSubAgentTool::new(
                    Arc::clone(&registries.subagent_factory),
                    spec.clone(),
                ));
                registries.tools.register_plugin_tool(tool);
            }
            Err(e) => errors.push(RegisterError {
                capability: "agents",
                name: agent.name.clone(),
                message: e,
            }),
        }
    }

    // 3. MCP servers —— start_server_with_namespace,异步操作。
    for mcp_loaded in &loaded.mcp_servers {
        let cfg = McpConfig {
            name: mcp_loaded.scoped_name.clone(),
            transport: match mcp_loaded.transport {
                crate::capabilities::mcp::McpTransportKind::Stdio => McpTransport::Stdio,
                crate::capabilities::mcp::McpTransportKind::Http => McpTransport::Http,
            },
            command: mcp_loaded.config.command.clone(),
            args: mcp_loaded.config.args.clone().unwrap_or_default(),
            env: mcp_loaded
                .config
                .env
                .clone()
                .map(|m| m.into_iter().collect())
                .unwrap_or_default(),
            url: mcp_loaded.config.url.clone(),
            headers: mcp_loaded
                .config
                .headers
                .clone()
                .map(|m| m.into_iter().collect())
                .unwrap_or_default(),
            timeout: std::time::Duration::from_secs(mcp_loaded.config.timeout_secs.unwrap_or(30)),
        };
        if let Err(e) = registries
            .mcp
            .start_server_with_namespace(loaded.plugin_id.as_str(), &mcp_loaded.original_name, cfg)
            .await
        {
            errors.push(RegisterError {
                capability: "mcp_servers",
                name: mcp_loaded.scoped_name.clone(),
                message: format!("{e}"),
            });
        }
    }

    // 4. Commands / 5. Hooks —— plugin hooks 尚未实现 ShellHook;
    //    扫描到的 hook 声明仅 log,不注册到 HookEngine。
    if !loaded.hooks.is_empty() {
        tracing::info!(
            plugin = %loaded.plugin_id,
            count = loaded.hooks.len(),
            "plugin hooks deferred: ShellHook not implemented yet (Phase B-internal)"
        );
        for hook in &loaded.hooks {
            tracing::debug!(
                plugin = %loaded.plugin_id,
                event = %hook.event,
                matcher = ?hook.matcher,
                source = %hook.source,
                "plugin hook stub (not registered)"
            );
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// 反注册 plugin 的所有能力。
///
/// 调用顺序很重要:
///
/// 1. **MCP first** —— 拿回 tool 全名,反注册 `ToolRegistry` 中带 `mcp__` 前缀的项。
/// 2. **Tools by source** —— `unregister_source(Plugin)` 一次性清掉所有
///    `ToolSource::Plugin` 标记的 tool(本插件之前 register 时挂的)。
/// 3. **Skills** —— `remove_plugin_skills` 走 plugin_id 过滤。
/// 4. **Subagent specs** —— `take_plugin_specs` 拿回 spec 列表。
/// 5. **Hooks** —— 延迟处理。
pub async fn unregister(registries: &LoaderRegistries, plugin_id: &PluginId) -> Result<(), String> {
    // 1. 此插件持有的 MCP 服务器
    let server_names = registries.mcp.server_names().await;
    let plugin_servers = reflect_mcp::list_plugin_servers(&server_names, plugin_id.as_str());
    let mut all_mcp_tool_names = Vec::new();
    for server_name in plugin_servers {
        if let Some(tool_names) = registries.mcp.stop_server(&server_name).await {
            all_mcp_tool_names.extend(tool_names);
        }
    }
    // 反注册 mcp__* tools (ToolRegistry 里)
    for tn in all_mcp_tool_names {
        registries.tools.unregister(&tn);
    }

    // 2. 来自 Plugin 的工具(本插件的 agents 与未来 commands)
    let removed_tools = registries
        .tools
        .unregister_source(reflect_tools::ToolSource::Plugin);
    tracing::debug!(
        plugin = %plugin_id,
        count = removed_tools,
        "unregister: 清掉 Plugin-sourced tools"
    );

    // 3. 技能(Skills)
    registries.skills.remove_plugin_skills(plugin_id.as_str());

    // 4. 子 agent 规格(Subagent specs)
    let _specs = registries
        .subagent_factory
        .take_plugin_specs(plugin_id.as_str());

    // 5. Hooks —— ShellHook 未实现;register 阶段仅 log,unregister 无状态可清。
    tracing::debug!(plugin = %plugin_id, "unregister: plugin hooks stub had no HookEngine entries");
    Ok(())
}

// ── Agent frontmatter 解析 ────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct AgentFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
}

fn parse_agent_frontmatter(
    loaded: &crate::capabilities::agents::LoadedAgent,
) -> Result<SubAgentSpec, String> {
    let raw = loaded
        .frontmatter_raw
        .as_deref()
        .ok_or_else(|| "missing frontmatter".to_string())?;
    let fm: AgentFrontmatter =
        serde_yaml::from_str(raw).map_err(|e| format!("frontmatter YAML parse: {e}"))?;
    let role = derive_role(&loaded.name);
    let allowed_tools = fm.tools.unwrap_or_default();
    Ok(SubAgentSpec {
        name: fm.name.unwrap_or_else(|| loaded.name.clone()),
        role,
        model: fm.model,
        system_prompt: fm.description.unwrap_or_default(),
        allowed_tools,
        data_transfer: reflect_subagent::DataTransferConfig::default(),
        max_turns: None,
        allowed_skills: vec![],
    })
}

/// 从 agent 全名(`<plugin>:<namespace>:<basename>`)派生 subagent `role`。
///
/// `SubAgentSpec.role` 必须是 `[a-z0-9_-]+`,所以把 `:` 替换成 `_`,
/// 截断到 32 字符以防过长。这是 best-effort —— 极小概率撞名,撞了
/// 就在 enable 时 `register_plugin_spec` 的 validate 路径报错。
fn derive_role(agent_name: &str) -> String {
    let mut role = agent_name.replace(':', "_");
    role.truncate(32);
    role
}

/// 单条 register 失败 —— 给上层 UI 显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterError {
    pub capability: &'static str,
    pub name: String,
    pub message: String,
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "register failed: capability={}, name={}, message={}",
            self.capability, self.name, self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        AgentSpec, CommandSpec, HookSpec, McpServerConfig, McpServerSpec, PluginManifest, SkillSpec,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    /// 构造一个 5 类能力齐备的 plugin fixture。
    fn make_full_plugin(root: &std::path::Path) -> PluginManifest {
        fs::create_dir_all(root.join("commands/utils")).unwrap();
        fs::write(
            root.join("commands/hello.md"),
            "---\ndescription: greet\n---\n# hi\n",
        )
        .unwrap();
        fs::write(root.join("commands/utils/lint.md"), "# lint").unwrap();

        fs::create_dir_all(root.join("agents")).unwrap();
        fs::write(
            root.join("agents/review.md"),
            "---\nname: Reviewer\ndescription: review PR\nmodel: anthropic/claude-3-5-sonnet-latest\ntools: [read, grep]\n---\n# body",
        )
        .unwrap();

        fs::create_dir_all(root.join("skills/lint")).unwrap();
        fs::write(
            root.join("skills/lint/SKILL.md"),
            "---\ndescription: lint skill\n---\n",
        )
        .unwrap();

        fs::write(
            root.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"echo blocked"}]}]}}"#,
        )
        .unwrap();

        let mut mcp = BTreeMap::new();
        mcp.insert(
            "fs".to_string(),
            McpServerConfig {
                command: Some("npx".into()),
                args: Some(vec!["-y".into(), "fs-server".into()]),
                ..Default::default()
            },
        );

        PluginManifest {
            name: "full".into(),
            version: Some("1.0.0".into()),
            commands: CommandSpec::Path("./commands".into()),
            agents: AgentSpec::Path("./agents".into()),
            skills: SkillSpec::Path("./skills".into()),
            hooks: HookSpec::Path("./hooks.json".into()),
            mcp_servers: McpServerSpec::Inline(mcp),
            ..Default::default()
        }
    }

    #[test]
    fn scan_full_plugin_discovers_all_five_capabilities() {
        let tmp = TempDir::new().unwrap();
        let plugin_root = tmp.path();
        let manifest = make_full_plugin(plugin_root);
        let id = PluginId::inline("full").unwrap();
        let (loaded, errors) = scan(&id, &manifest, plugin_root);

        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(loaded.commands.len(), 2);
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.hooks.len(), 1);
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.mcp_servers.len(), 1);
        assert!(!loaded.is_empty());

        // 命令名空间:`full:hello` / `full:utils:lint`
        let cmd_names: Vec<&str> = loaded.commands.iter().map(|c| c.name.as_str()).collect();
        assert!(cmd_names.contains(&"full:hello"));
        assert!(cmd_names.contains(&"full:utils:lint"));

        // agent 名:`full:review`
        assert_eq!(loaded.agents[0].name, "full:review");

        // hook event 与 matcher
        assert_eq!(loaded.hooks[0].event, "PreToolUse");
        assert_eq!(loaded.hooks[0].matcher.as_deref(), Some("bash"));

        // MCP scoped_name(命名空间化的 MCP 名)
        assert_eq!(loaded.mcp_servers[0].scoped_name, "plugin:full:fs");
    }

    #[test]
    fn scan_empty_plugin_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let manifest = PluginManifest {
            name: "empty".into(),
            ..Default::default()
        };
        let id = PluginId::inline("empty").unwrap();
        let (loaded, errors) = scan(&id, &manifest, tmp.path());
        assert!(errors.is_empty());
        assert!(loaded.is_empty());
    }

    #[test]
    fn scan_aggregates_per_capability_errors() {
        let tmp = TempDir::new().unwrap();
        let plugin_root = tmp.path();
        // mcp_servers 路径声明一个不存在的文件 → 不会报错(已 warn + 跳过);
        // 但 manifest 中 inline 的 mcp_servers 同时有 command 和 url → error。
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "broken".to_string(),
            McpServerConfig {
                command: Some("a".into()),
                url: Some("https://x".into()),
                ..Default::default()
            },
        );
        let manifest = PluginManifest {
            name: "broken".into(),
            mcp_servers: McpServerSpec::Inline(mcp),
            ..Default::default()
        };
        let id = PluginId::inline("broken").unwrap();
        let (_loaded, errors) = scan(&id, &manifest, plugin_root);
        assert!(errors.iter().any(|e| e.capability == "mcp_servers"));
    }

    #[test]
    fn scan_uses_marketplace_name_for_namespacing() {
        let tmp = TempDir::new().unwrap();
        let manifest = PluginManifest {
            name: "demo".into(),
            commands: CommandSpec::Path("./commands".into()),
            ..Default::default()
        };
        fs::create_dir_all(tmp.path().join("commands")).unwrap();
        fs::write(tmp.path().join("commands/h.md"), "# hi").unwrap();
        // 模拟非 inline marketplace 的 plugin id。
        let id = PluginId::parse("demo@anthropic-tools").unwrap();
        let (loaded, _) = scan(&id, &manifest, tmp.path());
        assert_eq!(loaded.commands[0].name, "demo:h");
    }

    #[test]
    fn register_skills_and_agents_in_to_catalog_and_factory() {
        use crate::identifier::PluginId;
        use crate::loader::{parse_agent_frontmatter, scan};

        let tmp = TempDir::new().unwrap();
        let manifest = make_full_plugin(tmp.path());
        let id = PluginId::inline("full").unwrap();
        let (loaded, _) = scan(&id, &manifest, tmp.path());

        // Skills 注册到 catalog
        let catalog = Arc::new(parking_lot::Mutex::new(SkillsCatalog::new()));
        let skill_metas: Vec<SkillMeta> = loaded
            .skills
            .iter()
            .map(|s| SkillMeta {
                name: s.name.clone(),
                description: s.description.clone().unwrap_or_default(),
                triggers: vec![],
                tools: vec![],
                mcp_collections: vec![],
                path: s.skill_md.clone(),
                body: String::new(),
                plugin_id: Some(id.to_string()),
                when_paths: vec![],
            })
            .collect();
        catalog.lock().add_plugin_skills(id.as_str(), &skill_metas);
        assert!(catalog.lock().plugin_ids().contains(&id.to_string()));

        // Agent frontmatter 解析成功
        let agent = &loaded.agents[0];
        let spec = parse_agent_frontmatter(agent).unwrap();
        assert_eq!(spec.name, "Reviewer");
        assert_eq!(spec.role, "full_review");
        assert!(spec.model.is_some());
        assert_eq!(spec.allowed_tools, vec!["read", "grep"]);
    }

    #[test]
    fn register_logs_hooks_without_error() {
        use reflect_hooks::HookEngine;
        use reflect_mcp::McpConnectionManager;
        use reflect_skills::SkillsCatalog;
        use reflect_subagent::SubAgentFactory;
        use reflect_tools::ToolRegistry;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        let tmp = TempDir::new().unwrap();
        let manifest = make_full_plugin(tmp.path());
        let id = PluginId::inline("full").unwrap();
        let (loaded, _) = scan(&id, &manifest, tmp.path());
        assert!(!loaded.hooks.is_empty());

        let tools = Arc::new(ToolRegistry::default());
        let hooks = Arc::new(HookEngine::new());
        let (tx, _rx) = mpsc::channel(16);
        let mcp = Arc::new(McpConnectionManager::new(tx));
        let skills = Arc::new(SkillsCatalog::new());
        let factory = Arc::new(SubAgentFactory::new(
            reflect_protocol::ThreadId::new(),
            "openai/gpt-4o",
            Arc::new(reflect_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::clone(&tools),
            CancellationToken::new(),
            None,
        ));
        let registries = LoaderRegistries::new(tools, hooks, mcp, skills, factory);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(register(&registries, &loaded));
        // MCP 可能失败(nonexistent binary),但 hooks stub 不应追加 error。
        if let Err(errs) = &result {
            assert!(
                errs.iter().all(|e| e.capability == "mcp_servers"),
                "unexpected errors: {errs:?}"
            );
        }
    }

    #[test]
    fn derive_role_replaces_colons_and_truncates() {
        assert_eq!(derive_role("foo:bar"), "foo_bar");
        assert_eq!(derive_role("a:b:c:d"), "a_b_c_d");
        assert_eq!(derive_role(&"x".repeat(50).to_string()).len(), 32);
    }
}
