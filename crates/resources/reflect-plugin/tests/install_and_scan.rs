//! 集成测试 —— `install_local` + `loader::scan` 全流程。
//!
//! Phase A 验收用例:从本地目录 install 一个 5 类能力齐备的 plugin,
//! 再调 `loader::scan` 验证所有 5 类都被发现。

use std::fs;
use tempfile::TempDir;

use reflect_plugin::loader::scan;
use reflect_plugin::manifest::{McpServerConfig, McpServerSpec, PluginManifest};
use reflect_plugin::state::PluginScope;
use reflect_plugin::{MarketplaceName, PluginId, PluginManager};

fn make_full_plugin_fixture(parent: &std::path::Path) -> std::path::PathBuf {
    let dir = parent.join("full");
    fs::create_dir_all(&dir).unwrap();

    // manifest:5 类全声明
    let toml = r#"
name = "full"
version = "1.0.0"
description = "5-capability fixture"

commands = "./commands"
agents = "./agents"
skills = "./skills"
hooks = "./hooks/hooks.json"

[mcp_servers.fs]
command = "npx"
args = ["-y", "fs-server"]

[mcp_servers.github]
url = "https://mcp.example.com/github"
"#;
    fs::write(dir.join("plugin.toml"), toml).unwrap();

    // 写命令文件 commands/*.md
    fs::create_dir_all(dir.join("commands/utils")).unwrap();
    fs::write(
        dir.join("commands/hello.md"),
        "---\ndescription: greet user\n---\n# hi\n",
    )
    .unwrap();
    fs::write(dir.join("commands/utils/lint.md"), "# lint").unwrap();

    // 写代理文件 agents/*.md
    fs::create_dir_all(dir.join("agents")).unwrap();
    fs::write(
        dir.join("agents/review.md"),
        "---\ndescription: review PR\n---\n# body",
    )
    .unwrap();

    // 写技能文件 skills/<name>/SKILL.md
    fs::create_dir_all(dir.join("skills/lint")).unwrap();
    fs::write(
        dir.join("skills/lint/SKILL.md"),
        "---\ndescription: lint skill\n---\n",
    )
    .unwrap();

    // 写钩子配置 hooks/hooks.json
    fs::create_dir_all(dir.join("hooks")).unwrap();
    fs::write(
        dir.join("hooks/hooks.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"echo blocked"}]}]}}"#,
    )
    .unwrap();

    // 写 MCP 配置 .mcp.json
    fs::write(
        dir.join(".mcp.json"),
        r#"{
            "fs": { "command": "npx", "args": ["-y", "fs-server"] },
            "github": { "url": "https://mcp.example.com/github" }
        }"#,
    )
    .unwrap();

    dir
}

#[test]
fn install_then_scan_finds_all_five_capabilities() {
    let tmp = TempDir::new().unwrap();
    let source_root = tmp.path().join("source");
    let source_dir = make_full_plugin_fixture(&source_root);
    let plugins_root = tmp.path().join("plugins");

    // 1. 安装
    let mut mgr = PluginManager::new(&plugins_root);
    let (id, entry) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    assert_eq!(id.as_str(), "full@inline");
    assert!(entry.install_path.exists());

    // 2. 重读 manifest(从 cache 安装目录)并扫描
    let cached_manifest_path = entry.install_path.join("plugin.toml");
    let cached_manifest = PluginManifest::from_path(&cached_manifest_path).unwrap();
    let (loaded, errors) = scan(&id, &cached_manifest, &entry.install_path);
    assert!(errors.is_empty(), "errors: {errors:?}");

    // 3. 5 类能力都发现
    assert_eq!(loaded.commands.len(), 2, "commands: {:?}", loaded.commands);
    assert_eq!(loaded.agents.len(), 1);
    assert_eq!(loaded.hooks.len(), 1);
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.mcp_servers.len(), 2);

    // 4. 命令命名空间
    let cmd_names: Vec<&str> = loaded.commands.iter().map(|c| c.name.as_str()).collect();
    assert!(cmd_names.contains(&"full:hello"));
    assert!(cmd_names.contains(&"full:utils:lint"));

    // 5. MCP scoped_name 格式
    let mcp_names: Vec<&str> = loaded
        .mcp_servers
        .iter()
        .map(|m| m.scoped_name.as_str())
        .collect();
    assert!(mcp_names.contains(&"plugin:full:fs"));
    assert!(mcp_names.contains(&"plugin:full:github"));

    // 6. agent 与 skill 命名
    assert_eq!(loaded.agents[0].name, "full:review");
    assert_eq!(loaded.skills[0].name, "lint");
}

#[test]
fn install_then_uninstall_clears_cache_and_scan_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let source_root = tmp.path().join("source");
    let source_dir = source_root.join("full");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("plugin.toml"), "name = \"mini\"\n").unwrap();

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let (id, entry) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    assert!(entry.install_path.exists());

    mgr.uninstall(&id, PluginScope::User).unwrap();
    assert!(!entry.install_path.exists());
    // state 已清空,无法再 scan。
    assert!(mgr.state().plugins.is_empty());
}

#[test]
fn scan_uses_id_marketplace_for_mcp_namespace() {
    // 非 inline marketplace 的 plugin id 同样参与 mcp scoped_name 构造。
    let tmp = TempDir::new().unwrap();
    let manifest = PluginManifest {
        name: "demo".into(),
        mcp_servers: McpServerSpec::Inline({
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "remote".to_string(),
                McpServerConfig {
                    url: Some("https://x".into()),
                    ..Default::default()
                },
            );
            m
        }),
        ..Default::default()
    };
    let id = PluginId::parse("demo@anthropic-tools").unwrap();
    let (loaded, _) = scan(&id, &manifest, tmp.path());
    assert_eq!(loaded.mcp_servers[0].scoped_name, "plugin:demo:remote");
}

#[test]
fn install_empty_plugin_succeeds_with_no_capabilities() {
    let tmp = TempDir::new().unwrap();
    let source = tmp.path().join("empty-plugin");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("plugin.toml"), "name = \"empty\"\n").unwrap();

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let (id, entry) = mgr
        .install_local(&source, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    assert_eq!(id.as_str(), "empty@inline");

    let manifest = PluginManifest::from_path(&entry.install_path.join("plugin.toml")).unwrap();
    let (loaded, errors) = scan(&id, &manifest, &entry.install_path);
    assert!(errors.is_empty());
    assert!(loaded.is_empty());
}
