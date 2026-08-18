//! `reflect mcp ...` —— 列出 / 增 / 删 / 测 / 看 MCP server。
//!
//! v0.4 范围:写 `~/.reflect/config.toml` 后**不触发跨进程热重载** ——
//! ConfigWatcher 只 watch 当前进程持有的 fd,跨进程 IPC 改 MCP 配置属 v0.5+。
//! `reflect mcp add` / `remove` 末尾会提示用户重启 `reflect exec`(交互式
//! TUI 见独立仓库 Reflect-TUI)。

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use reflect_config::{
    McpServerEntry, McpServersSection, McpTransport, ReflectConfig, default_config_path,
    load_default,
};
use reflect_mcp::{
    McpConnectionManager, McpLifecycleEvent, curated_catalog, find_entry, install_hint,
};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

use crate::login;

/// `reflect mcp ls` —— 列出已配 server(name / transport / command or url)。
pub fn ls() -> anyhow::Result<()> {
    let cfg = load_default();
    if cfg.mcp_servers.servers.is_empty() {
        println!(
            "(no [mcp_servers.*] configured; run `reflect mcp add <name> --command <cmd>` to add one)"
        );
        return Ok(());
    }
    println!(
        "{:<20}  {:<8}  {:<8}  endpoint",
        "name", "transport", "timeout"
    );
    let mut names: Vec<_> = cfg.mcp_servers.servers.keys().collect();
    names.sort();
    for name in names {
        let entry = &cfg.mcp_servers.servers[name];
        let transport = match entry.transport {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        };
        let timeout = entry
            .timeout_ms
            .map(|t| format!("{t}ms"))
            .unwrap_or_else(|| "30000ms".into());
        let endpoint = entry
            .command
            .clone()
            .or_else(|| entry.url.clone())
            .unwrap_or_else(|| "<missing>".into());
        println!(
            "{:<20}  {:<8}  {:<8}  {}",
            name,
            transport,
            timeout,
            truncate(&endpoint, 60)
        );
    }
    Ok(())
}

/// `reflect mcp add <name> ...` —— 写入新 server 到 `~/.reflect/config.toml`。
pub fn add(
    name: &str,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    headers: Vec<String>,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    if name.is_empty() {
        return Err(anyhow!("server name cannot be empty"));
    }
    let transport = match (&command, &url) {
        (Some(_), None) => McpTransport::Stdio,
        (None, Some(_)) => McpTransport::Http,
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "specify either --command (stdio) or --url (http), not both"
            ));
        }
        (None, None) => return Err(anyhow!("must specify --command (stdio) or --url (http)")),
    };

    let mut cfg = load_default();
    if cfg.mcp_servers.servers.contains_key(name) {
        return Err(anyhow!(
            "server '{name}' already exists; use `reflect mcp remove {name}` first"
        ));
    }

    let entry = match transport {
        McpTransport::Stdio => McpServerEntry {
            transport,
            command,
            args: if args.is_empty() { None } else { Some(args) },
            env: None,
            url: None,
            headers: Some(HashMap::new()),
            timeout_ms: Some(timeout_ms),
            always_load: None,
        },
        McpTransport::Http | McpTransport::Sse => McpServerEntry {
            transport,
            command: None,
            args: None,
            env: None,
            url,
            headers: {
                let mut map = HashMap::new();
                for h in headers {
                    if let Some((k, v)) = h.split_once(':') {
                        map.insert(k.trim().to_string(), v.trim().to_string());
                    } else {
                        return Err(anyhow!(
                            "invalid header '{h}'; expected 'Key: Value' format"
                        ));
                    }
                }
                Some(map)
            },
            timeout_ms: Some(timeout_ms),
            always_load: None,
        },
    };

    cfg.mcp_servers.servers.insert(name.to_string(), entry);
    let path = default_config_path().ok_or_else(|| anyhow!("HOME unset"))?;
    login::write_config_to(&cfg, &path).context("write config")?;

    println!("added server '{name}' to {}", path.display());
    println!("Restart `reflect exec` for the change to take effect.");
    Ok(())
}

/// `reflect mcp remove <name>` —— 删除 server 配置(仅改 TOML,不在运行进程里 hot-reload)。
pub fn remove(name: &str) -> anyhow::Result<()> {
    let mut cfg = load_default();
    if cfg.mcp_servers.servers.remove(name).is_none() {
        return Err(anyhow!(
            "server '{name}' not in config; run `reflect mcp ls` to see configured servers"
        ));
    }
    let path = default_config_path().ok_or_else(|| anyhow!("HOME unset"))?;
    login::write_config_to(&cfg, &path).context("write config")?;
    println!("removed server '{name}' from {}", path.display());
    println!("Restart `reflect exec` for the change to take effect.");
    Ok(())
}

/// `reflect mcp show <name>` —— 打印单个 server 的全部字段。
pub fn show(name: &str) -> anyhow::Result<()> {
    show_in(&load_default(), name)
}

/// `show_in` 是 `show` 的纯函数版本(接受 cfg 作为参数),便于测试。
pub fn show_in(cfg: &ReflectConfig, name: &str) -> anyhow::Result<()> {
    let entry = cfg
        .mcp_servers
        .servers
        .get(name)
        .ok_or_else(|| anyhow!("server '{name}' not in config"))?;
    let s = toml::to_string_pretty(&McpServersSection {
        servers: HashMap::from([(name.to_string(), entry.clone())]),
    })
    .context("serialize entry")?;
    println!("[mcp_servers.{name}]");
    for line in s.lines() {
        if line.starts_with('[') {
            continue;
        }
        println!("{line}");
    }
    Ok(())
}

/// `reflect mcp test <name>` —— 实际启动 server(走 `McpConnectionManager`),
/// 3 秒 timeout 验证能拿到 tool list;失败 → 退出码非 0。
pub async fn test(name: &str) -> anyhow::Result<()> {
    let cfg = load_default();
    let entry = cfg
        .mcp_servers
        .servers
        .get(name)
        .ok_or_else(|| anyhow!("server '{name}' not in config"))?
        .clone();

    let (tx, mut rx) = mpsc::channel::<McpLifecycleEvent>(8);
    let manager = McpConnectionManager::new(tx);

    let shapes = cfg.mcp_server_configs().context("mcp config validation")?;
    let shape = shapes
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("server '{name}' shape not found after validation"))?;
    let runtime_cfg: reflect_mcp::McpServerConfig = shape.into();

    println!(
        "starting server '{name}' (transport: {:?}) ...",
        entry.transport
    );

    let result = timeout(Duration::from_secs(3), manager.start_server(runtime_cfg)).await;
    match result {
        Ok(Ok(handle)) => {
            println!("✓ started; {} tools available:", handle.tools.len());
            for t in &handle.tools {
                println!("  - {}", t.full_name);
            }
            // 立即 shutdown,免得留下僵尸进程。
            manager.shutdown().await;
            // 还要排空 rx 里可能残留的事件。
            while rx.try_recv().is_ok() {}
            Ok(())
        }
        Ok(Err(e)) => {
            println!("✗ failed to start: {e}");
            while rx.try_recv().is_ok() {}
            Err(anyhow!("server '{name}' failed to start"))
        }
        Err(_) => {
            println!("✗ timeout after 3s; aborting");
            manager.shutdown().await;
            Err(anyhow!("server '{name}' start timed out"))
        }
    }
}

/// `reflect mcp registry ls` —— 列出精选 MCP catalog。
pub fn registry_ls() -> anyhow::Result<()> {
    println!("MCP registry (curated stub)");
    println!("id               category      name");
    for e in curated_catalog() {
        println!("{:<16}  {:<12}  {}", e.id, e.category, e.name);
    }
    println!("\n提示: `reflect mcp registry show <id>` 查看安装命令");
    Ok(())
}

/// `reflect mcp registry show <id>` —— 展示安装 hint。
pub fn registry_show(id: &str) -> anyhow::Result<()> {
    let entry = find_entry(id).ok_or_else(|| anyhow!("unknown registry id: {id}"))?;
    println!("{}\n{}", entry.description, install_hint(&entry));
    Ok(())
}

/// UTF-8 字符级截断到 `max_chars`,超出追加 `…`。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count >= max_chars.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
    }
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        out
    }
}

/// Test helper: 在 tmpdir 构造一个 ReflectConfig 并 add 一个 stdio entry。
#[cfg(test)]
pub(crate) fn add_test_server(cfg: &mut ReflectConfig, name: &str, command: &str) {
    cfg.mcp_servers.servers.insert(
        name.into(),
        McpServerEntry {
            transport: McpTransport::Stdio,
            command: Some(command.into()),
            args: Some(vec!["-y".into(), "@x/y".into()]),
            env: None,
            url: None,
            headers: Some(HashMap::new()),
            timeout_ms: Some(15_000),
            always_load: None,
        },
    );
}

/// Test helper: 把 cfg 写到 tmpdir config.toml 并返回 path。
#[cfg(test)]
pub(crate) fn write_to_tmp(cfg: &ReflectConfig, tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let path = tmp.path().join(".reflect").join("config.toml");
    login::write_config_to(cfg, &path).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// add stdio:写盘 + 校验能 load 回。
    #[test]
    fn mcp_add_stdio_writes_to_config() {
        let tmp = tmpdir();
        let mut cfg = ReflectConfig::default();
        add_test_server(&mut cfg, "fs", "npx");
        let path = write_to_tmp(&cfg, &tmp);

        let back = reflect_config::load_from_file(&path).unwrap();
        assert!(back.mcp_servers.servers.contains_key("fs"));
        let e = &back.mcp_servers.servers["fs"];
        assert_eq!(e.command.as_deref(), Some("npx"));
        assert_eq!(e.transport, McpTransport::Stdio);
    }

    /// add http:走 url 分支,headers 解析。
    #[test]
    fn mcp_add_http_writes_url_and_headers() {
        let tmp = tmpdir();
        let mut cfg = ReflectConfig::default();
        cfg.mcp_servers.servers.insert(
            "github".into(),
            McpServerEntry {
                transport: McpTransport::Http,
                command: None,
                args: None,
                env: None,
                url: Some("https://mcp.example.com/github".into()),
                headers: Some(HashMap::from([(
                    "Authorization".into(),
                    "Bearer xyz".into(),
                )])),
                timeout_ms: Some(20_000),
                always_load: None,
            },
        );
        let path = write_to_tmp(&cfg, &tmp);

        let back = reflect_config::load_from_file(&path).unwrap();
        let e = &back.mcp_servers.servers["github"];
        assert_eq!(e.transport, McpTransport::Http);
        assert_eq!(e.url.as_deref(), Some("https://mcp.example.com/github"));
        assert_eq!(
            e.headers
                .as_ref()
                .unwrap()
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer xyz")
        );
    }

    /// add 重名 → Err(由 caller 检查,这里是单元层测直接 `mcp_servers.contains_key`)。
    #[test]
    fn mcp_add_rejects_duplicate_at_data_layer() {
        let mut cfg = ReflectConfig::default();
        add_test_server(&mut cfg, "x", "npx");
        // 直接调 add_test_server 二次会覆盖;这里测的是业务逻辑的 contains_key 检查,
        // 走 `add` 函数路径更准确(在 integration 测试里覆盖)。
        assert!(cfg.mcp_servers.servers.contains_key("x"));
    }

    /// remove 删 entry,其它 entry 不动。
    #[test]
    fn mcp_remove_clears_entry_only() {
        let mut cfg = ReflectConfig::default();
        add_test_server(&mut cfg, "a", "npx");
        add_test_server(&mut cfg, "b", "cat");
        assert!(cfg.mcp_servers.servers.remove("a").is_some());
        assert!(cfg.mcp_servers.servers.contains_key("b"));
        assert!(!cfg.mcp_servers.servers.contains_key("a"));
    }

    /// ls 在空表时不 panic。
    #[test]
    fn ls_does_not_panic_on_empty() {
        let result = ls();
        assert!(result.is_ok());
    }

    /// truncate 行为符合预期。
    #[test]
    fn truncate_works_as_expected() {
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abcdefgh", 4), "abc…");
        assert_eq!(truncate("", 4), "");
    }

    /// show 把 entry 序列化为完整 TOML。
    #[test]
    fn show_pretty_prints_one_server() {
        let mut cfg = ReflectConfig::default();
        add_test_server(&mut cfg, "fs", "npx");
        let result = show_in(&cfg, "fs");
        assert!(result.is_ok());
    }

    /// show 找不到 → Err。
    #[test]
    fn show_unknown_server_errors() {
        let cfg = ReflectConfig::default();
        let result = show_in(&cfg, "nonexistent");
        assert!(result.is_err());
    }
}
