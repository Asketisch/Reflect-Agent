//! `reflect doctor` —— 环境自检,best-effort 报告多项,任一失败不阻塞。

use reflect_config::{default_config_path, load_default};
use reflect_rollout::index::list_sessions;

const OK: &str = "✓";
const WARN: &str = "!";

/// v0.4+: `reflect doctor` 子命令的实现。
///
/// 检查项(config / provider / rollout / MCP / LSP / plugins / rustc / sessions),
/// 整体 exit code 0(doctor 是诊断,不是校验)。
pub fn run(check_network: bool) -> anyhow::Result<()> {
    let cfg = load_default();
    let config_path = default_config_path();

    println!("Reflect doctor");
    println!("==================");

    // ── 1. 配置(config)──
    match &config_path {
        Some(p) if p.exists() => {
            println!("{OK} config    {} (parsed ok)", p.display());
        }
        Some(p) => {
            println!(
                "{WARN} config    {} (not present; using defaults)",
                p.display()
            );
        }
        None => {
            println!("{WARN} config    HOME unset; cannot locate ~/.reflect/config.toml");
        }
    }

    // ── 2. provider 可达性(provider reachability)──
    if check_network {
        if let Some(provider) = cfg.active_provider() {
            println!("[network] checking provider '{provider}' (best-effort)...");
            probe_provider_network(&cfg, provider);
        } else {
            println!("{WARN} provider  no [active].provider set; would skip network probe");
        }
    } else {
        println!("[skip]   provider  network probe skipped (pass --check-network to enable)");
    }

    // ── 3. rollout 目录(rollout dir)──
    let rollout_base = reflect_rollout::path::default_base();
    if rollout_base.exists() {
        println!(
            "{OK} rollout   {} (writable: assumed)",
            rollout_base.display()
        );
    } else {
        println!(
            "{WARN} rollout   {} does not exist yet; will be created on first session",
            rollout_base.display()
        );
    }

    // ── 4. MCP 服务器(MCP servers)──
    check_mcp(&cfg);

    // ── 5. LSP 服务器(LSP servers)──
    check_lsp(&cfg);

    // ── 6. 插件(Plugins)──
    check_plugins(&cfg);

    // ── 7. rustc 与 workspace 版本(rustc + workspace version)──
    // v1.6:探测加超时 + stdin 置 null —— `rustc` 可能是 rustup shim,
    // 空 HOME 下会触发工具链网络下载而无限阻塞(e2e step 12 实测挂死
    // 25 分钟);诊断命令不允许卡死整个 doctor。
    let rustc = run_probe_with_timeout("rustc", &["--version"], std::time::Duration::from_secs(5))
        .unwrap_or_else(|| "rustc not on PATH (or probe timed out)".to_string());
    println!("[env]    rustc     {rustc}");
    println!("[env]    package   reflect {}", env!("CARGO_PKG_VERSION"));

    // ── 8. session 计数(session count)──
    match list_sessions(&rollout_base) {
        Ok(sessions) => println!(
            "[data]   sessions  {} found under {}",
            sessions.len(),
            rollout_base.display()
        ),
        Err(e) => println!("{WARN} sessions  list_sessions failed: {e}"),
    }

    // ── 9. 集成桩(integration stubs,P2/P3)──
    check_integration_stubs(&cfg);

    println!();
    println!("Use `reflect login` to configure credentials.");
    println!("Use `reflect doctor --check-network` to also probe the provider.");
    Ok(())
}

/// 解析配置的 provider base_url,对主机做 TCP 连通性探测(3s 超时)。
///
/// 仅验证网络可达性,不发认证请求;对 401/403 之类无需区分(那是
/// `reflect exec` 真实调用阶段的事)。Ollama 走明文 http,云厂商走 https。
fn probe_provider_network(cfg: &reflect_config::ReflectConfig, provider: &str) {
    use std::net::TcpStream;
    use std::time::Duration;

    let Some(url) = active_base_url(cfg, provider) else {
        println!("{WARN} provider  no base_url resolved for '{provider}'");
        return;
    };
    let (host, port) = match parse_host_port(&url) {
        Ok(hp) => hp,
        Err(e) => {
            println!("{WARN} provider  malformed base_url '{url}': {e}");
            return;
        }
    };
    // DNS 解析 → 取首个地址 → 3s 超时 TCP 连接。失败不 panic,只告警。
    let addr_str = format!("{host}:{port}");
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(socket_addr) => {
                match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(3)) {
                    Ok(_) => println!("{OK} provider  {url} reachable (tcp connect ok)"),
                    Err(e) => println!("{WARN} provider  {url} unreachable: {e}"),
                }
            }
            None => println!("{WARN} provider  {addr_str} resolved to no address"),
        },
        Err(e) => println!("{WARN} provider  resolve {addr_str} failed: {e}"),
    }
}

/// 从 config + provider 推导 base_url,缺省走厂商默认 endpoint。
fn active_base_url(cfg: &reflect_config::ReflectConfig, provider: &str) -> Option<String> {
    let section_url = match provider {
        "anthropic" => cfg.anthropic.as_ref().and_then(|s| s.base_url.clone()),
        "openai" => cfg.openai.as_ref().and_then(|s| s.base_url.clone()),
        "ollama" => cfg.ollama.as_ref().and_then(|s| s.base_url.clone()),
        _ => None,
    };
    Some(section_url.unwrap_or_else(|| match provider {
        "anthropic" => "https://api.anthropic.com".to_string(),
        "openai" => "https://api.openai.com".to_string(),
        "ollama" => "http://127.0.0.1:11434".to_string(),
        other => other.to_string(),
    }))
}

/// 把 `https://host[:port]/path` 拆成 (host, port),协议缺省 https:443 / http:80。
fn parse_host_port(url: &str) -> anyhow::Result<(String, u16)> {
    let (scheme_stripped, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443u16)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80u16)
    } else {
        (url, 443u16)
    };
    // 取 authority 段(第一个 '/' 之前),丢掉 path / query。
    let authority = scheme_stripped.split('/').next().unwrap_or(scheme_stripped);
    let authority = authority.split('?').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        // 形如 host:port;IPv6 [::1]:port 的兜底由 to_socket_addrs 处理。
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        anyhow::bail!("empty host");
    }
    Ok((host, port))
}

// `to_socket_addrs` 需要 trait import(P2 `doctor-enhance` 网络探测)。
use std::net::ToSocketAddrs;

/// MCP 配置校验 + 摘要。
fn check_mcp(cfg: &reflect_config::ReflectConfig) {
    if cfg.mcp_servers.servers.is_empty() {
        println!("[skip]   mcp       no [mcp_servers.*] configured");
        return;
    }
    match cfg.mcp_server_configs() {
        Ok(configs) => {
            let stdio = configs
                .iter()
                .filter(|c| c.transport == reflect_config::McpTransport::Stdio)
                .count();
            let http = configs
                .iter()
                .filter(|c| c.transport == reflect_config::McpTransport::Http)
                .count();
            let sse = configs
                .iter()
                .filter(|c| c.transport == reflect_config::McpTransport::Sse)
                .count();
            println!(
                "{OK} mcp       {} server(s): stdio={stdio} http={http} sse={sse}",
                configs.len()
            );
            for c in &configs {
                let ty = match c.transport {
                    reflect_config::McpTransport::Stdio => "stdio",
                    reflect_config::McpTransport::Http => "http",
                    reflect_config::McpTransport::Sse => "sse",
                };
                let endpoint = c
                    .command
                    .as_deref()
                    .or(c.url.as_deref())
                    .unwrap_or("<missing>");
                println!("         - {} ({ty}) {endpoint}", c.name);
            }
            println!("         (run `reflect mcp test <name>` to validate runtime)");
        }
        Err(e) => {
            println!("{WARN} mcp       config invalid: {e}");
        }
    }
}

/// LSP 配置摘要。
fn check_lsp(cfg: &reflect_config::ReflectConfig) {
    if cfg.lsp_servers.servers.is_empty() {
        println!("[skip]   lsp       no [lsp_servers.*] configured");
        return;
    }
    match cfg.lsp_server_configs() {
        Ok(configs) => {
            println!("{OK} lsp       {} server(s)", configs.len());
            for c in &configs {
                println!(
                    "         - {} cmd={} patterns={}",
                    c.name,
                    c.command,
                    c.patterns.len()
                );
            }
        }
        Err(e) => {
            println!("{WARN} lsp       config invalid: {e}");
        }
    }
}

/// P2/P3 集成 stub 摘要行。
fn check_integration_stubs(cfg: &reflect_config::ReflectConfig) {
    use reflect_config::analytics_status_line;
    use reflect_integration::notifications::WebhookNotifier;
    use reflect_stream::PostgresSessionConfig;
    use reflect_stream::postgres_session;

    println!(
        "[stub]   notify    {}",
        WebhookNotifier::new(
            cfg.notifications
                .as_ref()
                .and_then(|n| n.webhook_url.clone())
        )
        .status_line()
    );
    let pg = cfg.postgres_session.as_ref().and_then(|p| {
        p.database_url.as_ref().map(|u| PostgresSessionConfig {
            database_url: u.clone(),
            table_prefix: p.table_prefix.clone(),
            pool_size: 4,
        })
    });
    println!(
        "[stub]   postgres  {}",
        postgres_session::status_line(pg.as_ref())
    );
    println!(
        "[v1.3]   analytics {}",
        analytics_status_line(cfg.analytics.as_ref())
    );
    println!(
        "[stub]   mcp-reg   {} catalog entries",
        reflect_mcp::curated_catalog().len()
    );
    let audit = reflect_integration::run_cargo_audit(None);
    println!("[stub]   security  {}", audit.summary);
}

/// 已安装 / 已启用 plugin 摘要。
fn check_plugins(cfg: &reflect_config::ReflectConfig) {
    let enabled = cfg.plugins.enabled_plugins.len();
    let root = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".reflect").join("plugins"));
    let installed = root
        .as_ref()
        .and_then(|p| reflect_plugin::PluginManager::load(p).ok())
        .map(|m| m.list_installed().len())
        .unwrap_or(0);
    if installed == 0 && enabled == 0 {
        println!("[skip]   plugins   none installed");
        return;
    }
    println!("{OK} plugins   installed={installed} enabled={enabled}");
    if enabled > 0 {
        for id in &cfg.plugins.enabled_plugins {
            println!("         - {id} (enabled)");
        }
    }
}

/// 测试辅助:加载 config 并快照解析摘要。
/// 带超时运行外部探测命令,取 stdout 首行(stdin 置 null 防交互提示)。
///
/// `rustc` / `cargo` 等 PATH 上的二进制可能是 rustup shim:空 HOME 下
/// shim 会触发工具链网络下载而无限阻塞 —— 诊断探测必须有界。
/// 超时 / 非零退出 / spawn 失败一律返回 `None`(doctor 是尽力而为)。
fn run_probe_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out.trim().to_string());
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
fn snapshot_summary(cfg: &reflect_config::ReflectConfig) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "anthropic={} openai={} ollama={}\n",
        cfg.anthropic.is_some(),
        cfg.openai.is_some(),
        cfg.ollama.is_some()
    ));
    out.push_str(&format!(
        "mcp_servers={} compact_trigger={:?}\n",
        cfg.mcp_servers.servers.len(),
        cfg.compact.trigger_tokens
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use reflect_config::{AnthropicSection, ReflectConfig};

    #[test]
    fn doctor_runs_without_panic_with_default() {
        let result = run(false);
        assert!(result.is_ok());
    }

    /// 快命令正常返回 stdout;卡死命令(超 sleep)超时后返回 None 并
    /// kill 子进程 —— 防 rustup shim 网络下载类挂死。
    #[test]
    fn probe_timeout_kills_stuck_command() {
        let out = run_probe_with_timeout(
            "/bin/echo",
            &["probe-ok"],
            std::time::Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("probe-ok"));

        let started = std::time::Instant::now();
        let out =
            run_probe_with_timeout("/bin/sleep", &["30"], std::time::Duration::from_millis(300));
        assert!(out.is_none(), "卡死命令应超时返回 None");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "必须及时 kill,不能等子进程自然退出"
        );

        // 不存在的命令 → None(spawn 失败)。
        assert!(
            run_probe_with_timeout(
                "/nonexistent-reflect-probe",
                &[],
                std::time::Duration::from_secs(5)
            )
            .is_none()
        );
    }

    #[test]
    fn snapshot_summary_lists_present_sections() {
        let cfg = ReflectConfig {
            anthropic: Some(AnthropicSection {
                api_key: Some("sk-x".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = snapshot_summary(&cfg);
        assert!(s.contains("anthropic=true"));
        assert!(s.contains("openai=false"));
        assert!(s.contains("ollama=false"));
    }

    #[test]
    fn config_serializes_for_inspection() {
        let cfg = ReflectConfig::default();
        let s = toml::to_string_pretty(&cfg)
            .context("serialize default cfg")
            .unwrap();
        assert!(s.contains("[active]"));
        assert!(s.contains("[compact]"));
    }

    #[test]
    fn parse_host_port_https_default_443() {
        let (h, p) = parse_host_port("https://api.anthropic.com").unwrap();
        assert_eq!(h, "api.anthropic.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_host_port_http_custom_port() {
        let (h, p) = parse_host_port("http://127.0.0.1:11434/v1/").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 11434);
    }

    #[test]
    fn parse_host_port_strips_path_and_query() {
        let (h, p) = parse_host_port("https://api.openai.com/chat?x=1").unwrap();
        assert_eq!(h, "api.openai.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_host_port_rejects_empty_host() {
        assert!(parse_host_port("https:///path").is_err());
    }

    #[test]
    fn active_base_url_uses_section_then_defaults() {
        let cfg = ReflectConfig {
            anthropic: Some(AnthropicSection {
                base_url: Some("https://my.proxy.example".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            active_base_url(&cfg, "anthropic").as_deref(),
            Some("https://my.proxy.example")
        );
        // 无 section 时回退到厂商默认 endpoint。
        assert_eq!(
            active_base_url(&cfg, "openai").as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(
            active_base_url(&cfg, "ollama").as_deref(),
            Some("http://127.0.0.1:11434")
        );
    }
}
