//! MCP 官方注册表 —— 精选常用 MCP server 目录(stub + 可安装模板)。

use serde::{Deserialize, Serialize};

/// 注册表中的一条 MCP server 模板。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegistryEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    /// stdio 启动命令(展示用)。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub category: String,
}

/// 内置精选 catalog(v1.x stub,不联网拉取)。
pub fn curated_catalog() -> Vec<McpRegistryEntry> {
    vec![
        McpRegistryEntry {
            id: "filesystem".into(),
            name: "Filesystem".into(),
            description: "本地文件读写 MCP server".into(),
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
            ],
            category: "core".into(),
        },
        McpRegistryEntry {
            id: "github".into(),
            name: "GitHub".into(),
            description: "GitHub API MCP server".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
            category: "integration".into(),
        },
        McpRegistryEntry {
            id: "fetch".into(),
            name: "Fetch".into(),
            description: "HTTP fetch MCP server".into(),
            command: "uvx".into(),
            args: vec!["mcp-server-fetch".into()],
            category: "web".into(),
        },
        McpRegistryEntry {
            id: "postgres".into(),
            name: "PostgreSQL".into(),
            description: "PostgreSQL 只读查询 MCP server".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@modelcontextprotocol/server-postgres".into()],
            category: "data".into(),
        },
    ]
}

/// 按 id 查找 catalog 条目。
pub fn find_entry(id: &str) -> Option<McpRegistryEntry> {
    curated_catalog().into_iter().find(|e| e.id == id)
}

/// 生成 `reflect mcp add` 等价 TOML 片段(不写盘)。
pub fn install_hint(entry: &McpRegistryEntry) -> String {
    format!(
        "# reflect mcp add {} --command {} {}\n[mcp_servers.{}]\ncommand = {:?}\nargs = {:?}",
        entry.id,
        entry.command,
        entry.args.join(" "),
        entry.id,
        entry.command,
        entry.args
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_filesystem() {
        assert!(find_entry("filesystem").is_some());
    }

    #[test]
    fn install_hint_contains_command() {
        let e = find_entry("github").unwrap();
        assert!(install_hint(&e).contains("mcp_servers"));
    }
}
