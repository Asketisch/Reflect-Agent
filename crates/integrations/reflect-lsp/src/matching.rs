//! 文件路径 → LSP server 路由。
//!
//! `LspTool::execute` 拿到 `file_path` 后,需要决定走哪个 server。
//! 路由策略:
//!
//! 1. 显式 `server` 参数(`LspTool::execute` 入参的 `server` 字段):
//!    直接查 handles,name 命中即可;命中不到 → `InvalidArgs`。
//! 2. 隐式 glob 匹配:遍历所有 server 的 `patterns`,看 `relative_path`
//!    (相对 workspace)是否命中;命中则取第一个 + warn(tracing)。
//! 3. 全不命中 → `None`(由 `LspTool::execute` 转 `ToolError::InvalidArgs`)。
//!
//! ## 多 server 命中歧义
//!
//! 实际场景少:同一种语言通常只挂一个 server。但用户可能用
//! `rust-analyzer` + 自定义 `rust-debug` 共存,这时 LLM 可以通过
//! 显式 `server` 参数消歧。
//!
//! 优先排序:本实现取 HashMap 顺序的第一个,**Phase A 接受**。Phase B
//! 引入"配置权重"或"按 workspace 路径"区分。

use std::path::{Path, PathBuf};

use crate::manager::LspServerHandle;

/// 路由结果:`(server_name, language_id)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteResult {
    pub server_name: String,
    pub language_id: String,
}

/// 给定文件路径与已注册 handles,挑出最合适的 server。
///
/// 优先用 `explicit_server`(若指定);否则遍历 handles 按 glob 命中第一个。
///
/// `workspace` 用于把 `file_path` 转成相对路径去跑 globset 匹配 —
/// glob 字符串是用户配置的,通常相对 workspace root(`**/*.rs` 等)。
pub fn pick_server_for(
    file_path: &Path,
    workspace: &Path,
    handles: &[LspServerHandle],
    explicit_server: Option<&str>,
) -> Option<RouteResult> {
    if let Some(name) = explicit_server {
        let h = handles.iter().find(|h| h.server_name == name)?;
        // 显式指定时,language_id 取该 server 第一个 pattern(用户显式兜底)。
        let lang = h
            .startup_config
            .patterns
            .first()
            .map(|p| p.language_id.clone())
            .unwrap_or_default();
        return Some(RouteResult {
            server_name: name.to_string(),
            language_id: lang,
        });
    }
    // 转 workspace-relative 路径。
    let rel = relative_path(file_path, workspace);
    // 遍历 handles,顺序按 name 字典序(HashMap 顺序不稳定,排一下保 reload diff 可重现)。
    let mut sorted: Vec<&LspServerHandle> = handles.iter().collect();
    sorted.sort_by(|a, b| a.server_name.cmp(&b.server_name));
    for h in sorted {
        for pat in &h.startup_config.patterns {
            if pat.glob.is_match(&rel) {
                return Some(RouteResult {
                    server_name: h.server_name.clone(),
                    language_id: pat.language_id.clone(),
                });
            }
        }
    }
    None
}

/// 计算 `file_path` 相对 `workspace` 的相对路径。
///
/// `file_path` 已是绝对路径(LSP tool 在 execute 阶段拼好);
/// 拼不出来(跨 workspace 边界)→ 退化为原 path,glob 失败时由
/// caller 报 `InvalidArgs`。
pub fn relative_path(file_path: &Path, workspace: &Path) -> PathBuf {
    file_path
        .strip_prefix(workspace)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompiledFilePattern;
    use crate::manager::LspServerHandle;
    use std::collections::HashMap;
    use std::time::Duration;

    use lsp_types::ServerCapabilities;
    use tokio_util::sync::CancellationToken;

    fn make_handle(name: &str, patterns: &[(&str, &str)]) -> LspServerHandle {
        use std::collections::HashMap as H;
        use std::sync::Arc;
        let cfg = crate::LspServerConfig {
            name: name.to_string(),
            command: "x".to_string(),
            args: vec![],
            env: H::new(),
            patterns: patterns
                .iter()
                .map(|(g, l)| CompiledFilePattern::compile(g, l.to_string()).unwrap())
                .collect(),
            root_uri: None,
            initialization_options: None,
            timeout: Duration::from_secs(30),
        };
        LspServerHandle {
            server_name: name.to_string(),
            client: Arc::new(crate::client::LspClientInner {
                peer: Arc::new(crate::transport::LspPeer::stub_for_tests()),
                server_name: name.to_string(),
                server_info: None,
                capabilities: ServerCapabilities::default(),
                cancel: CancellationToken::new(),
                docs: parking_lot::RwLock::new(HashMap::new()),
                timeout: Duration::from_secs(30),
            }),
            supported_methods: vec![],
            language_ids: patterns.iter().map(|(_, l)| l.to_string()).collect(),
            startup_config: cfg,
        }
    }

    #[test]
    fn explicit_server_overrides_glob() {
        let h = vec![
            make_handle("rust", &[("**/*.rs", "rust")]),
            make_handle("go", &[("**/*.go", "go")]),
        ];
        let r = pick_server_for(
            Path::new("/ws/src/main.rs"),
            Path::new("/ws"),
            &h,
            Some("go"),
        )
        .unwrap();
        assert_eq!(r.server_name, "go");
        assert_eq!(r.language_id, "go");
    }

    #[test]
    fn explicit_server_unknown_returns_none() {
        let h = vec![make_handle("rust", &[("**/*.rs", "rust")])];
        let r = pick_server_for(
            Path::new("/ws/src/main.rs"),
            Path::new("/ws"),
            &h,
            Some("ghost"),
        );
        assert!(r.is_none());
    }

    #[test]
    fn glob_match_picks_first_server_sorted_by_name() {
        let h = vec![
            make_handle("ts", &[("**/*.ts", "typescript")]),
            make_handle("rs", &[("**/*.rs", "rust")]),
        ];
        let r = pick_server_for(Path::new("/ws/src/main.rs"), Path::new("/ws"), &h, None).unwrap();
        // 字典序优先 → "rs" 在 "ts" 之前
        assert_eq!(r.server_name, "rs");
        assert_eq!(r.language_id, "rust");
    }

    #[test]
    fn no_match_returns_none() {
        let h = vec![make_handle("rust", &[("**/*.rs", "rust")])];
        let r = pick_server_for(Path::new("/ws/README.md"), Path::new("/ws"), &h, None);
        assert!(r.is_none());
    }

    #[test]
    fn relative_path_strips_workspace_prefix() {
        let p = relative_path(Path::new("/ws/src/main.rs"), Path::new("/ws"));
        assert_eq!(p, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn relative_path_falls_back_when_outside_workspace() {
        let p = relative_path(Path::new("/other/main.rs"), Path::new("/ws"));
        assert_eq!(p, PathBuf::from("/other/main.rs"));
    }
}
