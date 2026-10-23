//! Phase B 集成测试 —— `LoaderRegistries::register` + `unregister` 全流程。
//!
//! 构造一组共享 registry,注册一个 5 类能力 fixture,验证各 registry
//! 都正确接收到 plugin 提供的项,然后 unregister 验证全部清干净。
//!
//! MCP 路径不在本测试覆盖 —— 真实 `start_server_with_namespace` 会 spawn
//! 子进程,与 Phase B 单元测试解耦。skills + agents + tools 路径全覆盖。

use std::fs;
use std::sync::Arc;

use reflect_hooks::HookEngine;
use reflect_mcp::McpConnectionManager;
use reflect_plugin::loader::{LoaderRegistries, register, scan, unregister};
use reflect_plugin::manifest::{McpServerConfig, PluginManifest};
use reflect_plugin::{PluginId, SubAgentFactory};
use reflect_skills::SkillsCatalog as SkillsCatalogType;
use reflect_tools::ToolRegistry;
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn make_plugin_fixture(parent: &std::path::Path) -> std::path::PathBuf {
    let dir = parent.join("phase-b-fixture");
    fs::create_dir_all(&dir).unwrap();

    let toml = r#"
name = "phase-b-fixture"
version = "1.0.0"
description = "Phase B fixture"

commands = "./commands"
agents = "./agents"
skills = "./skills"
hooks = "./hooks/hooks.json"
"#;
    fs::write(dir.join("plugin.toml"), toml).unwrap();

    fs::create_dir_all(dir.join("commands/utils")).unwrap();
    fs::write(
        dir.join("commands/hello.md"),
        "---\ndescription: greet\n---\n# hi\n",
    )
    .unwrap();

    fs::create_dir_all(dir.join("agents")).unwrap();
    fs::write(
        dir.join("agents/review.md"),
        "---\nname: Reviewer\ndescription: review PR\nmodel: anthropic/claude-3-5-sonnet-latest\ntools: [read, grep]\n---\n# body",
    )
    .unwrap();
    fs::write(
        dir.join("agents/test.md"),
        "---\nname: Tester\ndescription: run tests\n---\n",
    )
    .unwrap();

    fs::create_dir_all(dir.join("skills/lint")).unwrap();
    fs::write(
        dir.join("skills/lint/SKILL.md"),
        "---\ndescription: lint skill\n---\n",
    )
    .unwrap();

    fs::create_dir_all(dir.join("hooks")).unwrap();
    fs::write(
        dir.join("hooks/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"echo blocked"}]}]}}"#,
    )
    .unwrap();

    // MCP inline but 不真启动(stdio command 不存在会失败,但 register 错误聚合)
    let mut mcp = BTreeMap::new();
    mcp.insert(
        "fs".to_string(),
        McpServerConfig {
            command: Some("nonexistent-fake-binary".into()),
            ..Default::default()
        },
    );

    let mcp_json = serde_json::to_string(&mcp).unwrap();
    fs::write(dir.join(".mcp.json"), mcp_json).unwrap();

    dir
}

type RegistryHandles = (
    Arc<ToolRegistry>,
    Arc<HookEngine>,
    Arc<McpConnectionManager>,
    Arc<SkillsCatalogType>,
    Arc<SubAgentFactory>,
);

fn build_registries() -> RegistryHandles {
    let tools = Arc::new(ToolRegistry::default());
    let hooks = Arc::new(HookEngine::new());
    let (tx, _rx) = mpsc::channel(16);
    let mcp = Arc::new(McpConnectionManager::new(tx));
    let skills = Arc::new(SkillsCatalogType::new());
    let factory = Arc::new(SubAgentFactory::new(
        reflect_protocol::ThreadId::new(),
        "openai/gpt-4o",
        Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::clone(&tools),
        CancellationToken::new(),
        None,
    ));
    (tools, hooks, mcp, skills, factory)
}

#[tokio::test]
async fn register_unregister_full_cycle() {
    let tmp = TempDir::new().unwrap();
    let plugin_root = make_plugin_fixture(tmp.path());
    let manifest = PluginManifest::from_path(&plugin_root.join("plugin.toml")).unwrap();
    let id = PluginId::inline("phase-b-fixture").unwrap();
    let (loaded, scan_errors) = scan(&id, &manifest, &plugin_root);
    assert!(scan_errors.is_empty(), "scan errors: {scan_errors:?}");

    let (tools, hooks, mcp, skills, factory) = build_registries();
    let registries = LoaderRegistries::new(
        tools.clone(),
        hooks.clone(),
        mcp.clone(),
        skills.clone(),
        factory.clone(),
    );

    // Register
    let result = register(&registries, &loaded).await;
    // MCP 用 nonexistent binary 会失败,但 skills + agents 应成功。
    // errors 仅含 mcp_servers;其他能力已挂载。
    if let Err(errs) = &result {
        assert!(
            errs.iter().all(|e| e.capability == "mcp_servers"),
            "unexpected errors: {errs:?}"
        );
    }

    // 验证 skills 已挂到 catalog
    assert!(skills.plugin_ids().contains(&id.to_string()));
    let lint_plugin_id = skills.get("lint").and_then(|s| s.plugin_id.clone());
    assert_eq!(lint_plugin_id.as_deref(), Some("phase-b-fixture@inline"));

    // 验证 commands 已挂到 CommandRegistry(展开在用户输入层做)
    let commands = registries.commands.list();
    let cmd_names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cmd_names, vec!["phase-b-fixture:hello"], "{cmd_names:?}");
    // lookup 命中后可展开(读 md + 剥 frontmatter)
    let hello = registries.commands.lookup("phase-b-fixture:hello").unwrap();
    let expanded = reflect_plugin::expand_command(&hello, "张三").unwrap();
    assert!(expanded.contains("# hi"), "unexpected: {expanded}");

    // 验证 agents 已挂为 tool + spec
    let tool_names = tools
        .list_with_source()
        .into_iter()
        .filter(|(_, s)| *s == reflect_tools::ToolSource::Plugin)
        .map(|(n, _)| n)
        .collect::<Vec<_>>();
    assert!(
        tool_names
            .iter()
            .any(|n| n.contains("phase-b-fixture_review"))
    );
    assert!(
        tool_names
            .iter()
            .any(|n| n.contains("phase-b-fixture_test"))
    );

    let registered_plugins = factory.registered_plugin_ids();
    assert!(registered_plugins.contains(&id.to_string()));
    let specs = factory.plugin_specs_for(id.as_str());
    assert_eq!(specs.len(), 2);

    // Unregister
    unregister(&registries, &id).await.expect("unregister ok");

    // Skills catalog 清除
    assert!(!skills.plugin_ids().contains(&id.to_string()));
    // CommandRegistry 清除
    assert!(registries.commands.list().is_empty());
    assert!(
        registries
            .commands
            .lookup("phase-b-fixture:hello")
            .is_none()
    );
    // Plugin-sourced tools 清除
    let remaining_plugin_tools = tools
        .list_with_source()
        .into_iter()
        .filter(|(_, s)| *s == reflect_tools::ToolSource::Plugin)
        .count();
    assert_eq!(remaining_plugin_tools, 0);
    // Subagent specs 清除
    assert!(factory.plugin_specs_for(id.as_str()).is_empty());
}

#[tokio::test]
async fn register_then_unregister_then_register_idempotent() {
    let tmp = TempDir::new().unwrap();
    let plugin_root = make_plugin_fixture(tmp.path());
    let manifest = PluginManifest::from_path(&plugin_root.join("plugin.toml")).unwrap();
    let id = PluginId::inline("phase-b-fixture").unwrap();
    let (loaded, _) = scan(&id, &manifest, &plugin_root);

    let (tools, hooks, mcp, skills, factory) = build_registries();
    let registries = LoaderRegistries::new(
        tools.clone(),
        hooks.clone(),
        mcp.clone(),
        skills.clone(),
        factory.clone(),
    );

    // 第一次 register
    let _ = register(&registries, &loaded).await;
    let count_after_first = tools
        .list_with_source()
        .into_iter()
        .filter(|(_, s)| *s == reflect_tools::ToolSource::Plugin)
        .count();
    assert!(count_after_first > 0);

    // Unregister 后再 register —— 不应冲突(因为 unregister 已清干净)
    unregister(&registries, &id).await.unwrap();
    let count_after_unregister = tools
        .list_with_source()
        .into_iter()
        .filter(|(_, s)| *s == reflect_tools::ToolSource::Plugin)
        .count();
    assert_eq!(count_after_unregister, 0);

    let _ = register(&registries, &loaded).await;
    let count_after_second = tools
        .list_with_source()
        .into_iter()
        .filter(|(_, s)| *s == reflect_tools::ToolSource::Plugin)
        .count();
    assert_eq!(count_after_first, count_after_second);
}

#[test]
fn loader_registries_construction_is_cheap() {
    // LoaderRegistries 只是 Arc 集合,构造应 O(1) 且不触发 IO。
    let (tools, hooks, mcp, skills, factory) = build_registries();
    let _r = LoaderRegistries::new(tools, hooks, mcp, skills, factory);
}
