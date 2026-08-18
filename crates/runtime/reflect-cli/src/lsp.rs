//! `reflect lsp ...` —— 列出 / 状态查询 LSP server。
//!
//! v0.5 Phase A 范围:从 `~/.reflect/config.toml` 读 `[lsp_servers.*]`
//! 段,展示静态信息(命令 / globs / language id / timeout);`status <name>`
//! 显示某 server 的强类型转换结果。
//!
//! 与 `mcp` 区别:LSP 不支持 `add` / `remove` / `test`(v0.5 不做跨进程
//! 热重载,用户直接编辑 `config.toml` 重启更直观)。

use reflect_config::{ReflectConfig, load_default};

/// `reflect lsp list` —— 列出已配 server。
pub fn list() -> anyhow::Result<()> {
    let cfg = load_default();
    print_servers(&cfg);
    Ok(())
}

/// 共享的打印逻辑(单元测试也可复用)。
pub(crate) fn print_servers(cfg: &ReflectConfig) {
    if cfg.lsp_servers.servers.is_empty() {
        println!(
            "(no [lsp_servers.*] configured; add a section like:\n  [lsp_servers.rust]\n  command = \"rust-analyzer\"\n  file_patterns = [{{ glob = \"**/*.rs\", language_id = \"rust\" }}])"
        );
        return;
    }
    println!(
        "{:<16}  {:<24}  {:<8}  patterns",
        "name", "command", "timeout"
    );
    let mut names: Vec<_> = cfg.lsp_servers.servers.keys().collect();
    names.sort();
    for name in names {
        let entry = &cfg.lsp_servers.servers[name];
        let timeout = entry
            .timeout_ms
            .map(|t| format!("{t}ms"))
            .unwrap_or_else(|| "30000ms".into());
        let patterns: Vec<String> = entry
            .file_patterns
            .iter()
            .map(|p| format!("{}:{}", p.glob, p.language_id))
            .collect();
        println!(
            "{:<16}  {:<24}  {:<8}  [{}]",
            name,
            truncate(&entry.command, 24),
            timeout,
            patterns.join(", ")
        );
    }
}

/// `reflect lsp status <name>` —— 显示某 server 的强类型转换结果。
pub fn status(name: &str) -> anyhow::Result<()> {
    let cfg = load_default();
    let entry = cfg
        .lsp_servers
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("lsp server '{name}' not configured"))?;
    println!("name    : {name}");
    println!("command : {}", entry.command);
    if !entry.args.is_empty() {
        println!("args    : {}", entry.args.join(" "));
    }
    if !entry.env.is_empty() {
        println!("env     :");
        for (k, v) in &entry.env {
            println!("    {k}={v}");
        }
    }
    println!("timeout : {}ms", entry.timeout_ms.unwrap_or(30_000));
    if let Some(root) = &entry.root_uri {
        println!("root_uri: {root}");
    }
    if let Some(opts) = &entry.initialization_options {
        println!("init_opts: {opts}");
    }
    println!("patterns:");
    for p in &entry.file_patterns {
        println!("    {} → {}", p.glob, p.language_id);
    }
    // 强类型校验:走 reflect-config 校验一遍,有错就报。
    let shapes = cfg
        .lsp_server_configs()
        .map_err(|e| anyhow::anyhow!("config validation failed: {e}"))?;
    if let Some(shape) = shapes.iter().find(|s| s.name == name) {
        println!(
            "validated: ok ({} patterns, root_uri={:?})",
            shape.patterns.len(),
            shape.root_uri
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
