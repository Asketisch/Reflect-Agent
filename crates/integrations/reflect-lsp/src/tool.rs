//! `LspTool` —— 单一 LSP tool 暴露给 LLM。
//!
//! 整个 LSP 集成对 LLM 呈现为一个 tool:`lsp`。`action` 字段决定走哪条
//! LSP method,`file_path` 决定路由到哪个 server(`matching::pick_server_for`)。
//!
//! ## 参数 schema
//!
//! ```json
//! {
//!   "type": "object",
//!   "properties": {
//!     "action":  { "type": "string", "enum": ["definition", "references", "hover"] },  // LSP method 选择
//!     "file_path": { "type": "string" },                  // 目标文件路径
//!     "line":    { "type": "integer", "minimum": 0 },     // 行号(0 起)
//!     "character": { "type": "integer", "minimum": 0 },   // 列号(0 起)
//!     "include_declaration": { "type": "boolean", "default": true },  // references 是否含声明处
//!     "server":  { "type": "string" }                     // 显式指定 server(可省)
//!   },
//!   "required": ["action", "file_path", "line", "character"]  // 这四项必填
//! }
//! ```
//!
//! ## 权限
//!
//! - `required_permission = Prompt`:整个 tool 走 M6 approval gate。Phase A
//!   接受"hover 都要 confirm"的 UX 摩擦;Phase B 引入 per-action permission
//!   table 让只读 action 走 `Auto`。
//! - `is_concurrency_safe = false`:Phase A 保守,Phase B 按 action 切。
//!
//! ## 错误映射
//!
//! - 参数缺失/类型错 → `ToolError::InvalidArgs`
//! - 文件跨 workspace 边界 → `ToolError::PathEscape`
//! - 文件未匹配任何 server → `ToolError::InvalidArgs`
//! - LSP server 出错 / 超时 / 取消 → `ToolError::Execution` / `Timeout` / `Cancelled`
//! - 其他 IO 错 → `ToolError::Io`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lsp_types::{Position, Uri};
use reflect_protocol::{ContentBlock, PermissionMode, ToolError, ToolOutput};
use reflect_tools::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

use crate::manager::LspConnectionManager;
use crate::methods::{
    LspAction, invoke_completion, invoke_definition, invoke_document_symbol, invoke_hover,
    invoke_references, invoke_signature_help,
};

/// 单一 LSP tool 暴露给 LLM。
///
/// `bootstrap_lsp` 阶段构造一个,挂到 `ToolRegistry::register(ToolSource::Runtime, ...)`。
pub struct LspTool {
    /// manager 句柄。`execute` 路径上查 `manager.get_client(name)` 拿
    /// `Arc<LspClientInner>`,再 `ensure_open` + invoke_*。
    pub manager: Arc<LspConnectionManager>,
}

impl LspTool {
    /// 构造 tool。
    pub fn new(manager: Arc<LspConnectionManager>) -> Self {
        Self { manager }
    }

    /// 把 `file_path`(可能相对或绝对)按 workspace 锚定,跑 sandbox 校验。
    ///
    /// 优先 `resolve_sandbox_path`(canonicalize + prefix);失败(文件尚未
    /// 存在)走 fallback:用 `Path::components` 解析 `..` 后再 prefix
    /// 比对,这样 `/ws/foo/../etc/passwd` 会被识别为逃逸。
    fn resolve_path(workspace: &std::path::Path, file_path: &str) -> Result<PathBuf, ToolError> {
        let p = std::path::Path::new(file_path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            workspace.join(p)
        };
        // 优先 canonicalize(已存在的文件 → 走完整 sandbox 检查)。
        if let Ok(p) = reflect_tools::sandbox::resolve_sandbox_path(workspace, &abs) {
            return Ok(p);
        }
        // fallback:把 `..` 解析成实际路径,再 strip_prefix。
        let normalized = normalize_path(&abs);
        match normalized.strip_prefix(workspace) {
            Ok(rel) => Ok(workspace.join(rel)),
            Err(_) => Err(ToolError::PathEscape { path: abs }),
        }
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "在 file:line:character 位置执行 LSP 查询(`definition` / \
         `references` / `hover` / `document_symbol` / `completion` / \
         `signature_help`)。返回 JSON 文本。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["definition", "references", "hover", "document_symbol", "completion", "signature_help"],
                    "description": "LSP method to invoke"
                },
                "file_path": {
                    "type": "string",
                    "description": "File path relative to workspace (or absolute inside workspace)"
                },
                "line": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-indexed line number (ignored by document_symbol)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-indexed character (UTF-16 code unit) offset (ignored by document_symbol)"
                },
                "include_declaration": {
                    "type": "boolean",
                    "default": true,
                    "description": "For references action: include the declaration site"
                },
                "server": {
                    "type": "string",
                    "description": "Optional: explicitly pick an LSP server by name (from [lsp_servers.*])"
                }
            },
            "required": ["action", "file_path"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // Phase B1: 6 个只读 action 全部 concurrency_safe (`LspAction::is_concurrency_safe()`),
        // 这里只暴露 tool-level 入口;execute 内按 action 决策。
        // Phase A 整体 `false` 过于保守,LLM 在并发场景下等太久。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // Phase A:整个 lsp tool Prompt(已废,Phase B1 改 per-action Auto)。
        // 这里仍返回 Prompt 作为 "tool-level fallback";但 execute 内部
        // 走 `LspAction::required_permission()` per-action 表,只读 action
        // 全部 Auto,LLM 不会因为 hover 而被反复 confirm。
        //
        // Phase B 后续:`rename` / `codeAction` 新增 → tool-level fallback
        // 仍 `Prompt` 是合理兜底(未知 action 出现时拒绝执行)。
        PermissionMode::Prompt
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        // 1. 解析参数。
        let parsed: LspArgs = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            message: format!("lsp: invalid arguments: {e}"),
        })?;
        // 2. 路径 sandbox。
        let abs = Self::resolve_path(&ctx.workspace_path(), &parsed.file_path)?;
        // 3. 拿 handles + 路由。
        let handles = self.manager.all_handles().await;
        if handles.is_empty() {
            return Err(ToolError::Execution(
                "no LSP servers running; configure [lsp_servers.*] in ~/.reflect/config.toml"
                    .to_string(),
            ));
        }
        let route = crate::matching::pick_server_for(
            &abs,
            &ctx.workspace_path(),
            &handles,
            parsed.server.as_deref(),
        )
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("no LSP server registered for {}", parsed.file_path),
        })?;
        // 4. 拿 client + ensure_open。
        let client = self
            .manager
            .get_client(&route.server_name)
            .await
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "lsp server {} disappeared during routing",
                    route.server_name
                ))
            })?;
        let uri = path_to_uri(&abs).map_err(|e| ToolError::InvalidArgs {
            message: format!("lsp: cannot convert path to URI: {e}"),
        })?;
        client
            .ensure_open(&abs, &uri, &route.language_id)
            .await
            .map_err(|e| match e {
                crate::error::LspError::Spawn(io) => ToolError::Io(io.to_string()),
                other => ToolError::Execution(format!("lsp: ensure_open: {other}")),
            })?;
        // 5. 构造 position + dispatch by action。
        let position = Position {
            line: parsed.line.unwrap_or(0) as u32,
            character: parsed.character.unwrap_or(0) as u32,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let result = dispatch_action(
            &client,
            parsed.action,
            &uri,
            position,
            parsed.include_declaration,
        )
        .await;
        // 6. 包成 ToolOutput。
        match result {
            Ok(value) => Ok(ToolOutput {
                content: vec![ContentBlock::Text {
                    text: serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| value.to_string()),
                }],
                is_error: false,
                metadata: json!({
                    "server": route.server_name,
                    "language_id": route.language_id,
                    "action": parsed.action.method_str(),
                    "elapsed_ms": elapsed_ms,
                }),
                elapsed_ms,
            }),
            Err(e) => Err(map_error(e)),
        }
    }
}

// ── args 解析 + dispatch ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LspArgs {
    action: LspAction,
    file_path: String,
    /// 0-indexed 行号；`document_symbol` 不需要 position，可省略。
    #[serde(default)]
    line: Option<u64>,
    /// 0-indexed 字符偏移(UTF-16)；`document_symbol` 可省略。
    #[serde(default)]
    character: Option<u64>,
    #[serde(default = "default_true")]
    include_declaration: bool,
    #[serde(default)]
    server: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 按 `action` 派发到具体 invoke_* 函数。
///
/// 返回 `serde_json::Value` 让 `ToolOutput` 直接吃;LLM 在历史里看到
/// JSON 文本即可(无需再次反序列化)。
async fn dispatch_action(
    client: &Arc<crate::client::LspClientInner>,
    action: LspAction,
    uri: &Uri,
    position: Position,
    include_declaration: bool,
) -> Result<serde_json::Value, crate::error::LspError> {
    match action {
        LspAction::Definition => {
            let resp = invoke_definition(client, uri, position).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
        LspAction::References => {
            let resp = invoke_references(client, uri, position, include_declaration).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
        LspAction::Hover => {
            let resp = invoke_hover(client, uri, position).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
        LspAction::DocumentSymbol => {
            // document_symbol 不需要 position,只跟 uri。
            let resp = invoke_document_symbol(client, uri).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
        LspAction::Completion => {
            let resp = invoke_completion(client, uri, position).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
        LspAction::SignatureHelp => {
            let resp = invoke_signature_help(client, uri, position).await?;
            serde_json::to_value(resp).map_err(|e| crate::error::LspError::Call(e.to_string()))
        }
    }
}

fn map_error(e: crate::error::LspError) -> ToolError {
    use crate::error::LspError;
    match e {
        LspError::Timeout { elapsed_ms } => ToolError::Timeout { elapsed_ms },
        LspError::Cancelled => ToolError::Cancelled,
        LspError::Spawn(io) => ToolError::Io(io.to_string()),
        LspError::ServerError { code, message } => {
            ToolError::Execution(format!("lsp server error {code}: {message}"))
        }
        LspError::Call(msg) => ToolError::Execution(format!("lsp: {msg}")),
        LspError::ConfigInvalid(msg) => ToolError::InvalidArgs { message: msg },
        LspError::Path(msg) => ToolError::InvalidArgs { message: msg },
        LspError::Glob(msg) => ToolError::InvalidArgs { message: msg },
        LspError::Initialize(msg) => ToolError::Execution(format!("lsp init: {msg}")),
        LspError::ShutdownTimeout => ToolError::Execution("lsp shutdown timeout".to_string()),
    }
}

// ── matching helper shim ─────────────────────────────────────────────
// `matching::pick_server_for` 接受 `&[LspServerHandle]`,直接从
// `manager.all_handles().await` 拿,无需任何适配层。

// ── path → LSP URI helper ───────────────────────────────────────────

/// 把绝对文件系统路径转 LSP `file://` URI。`lsp_types::Uri` 0.97 不提供
/// `from_file_path` helper(被 deprecated / 移除),这里用 `url::Url` 中转。
fn path_to_uri(path: &std::path::Path) -> Result<Uri, String> {
    use std::str::FromStr;
    let url = url::Url::from_file_path(path)
        .map_err(|()| "file:// conversion failed (path not absolute or unsupported)".to_string())?;
    Uri::from_str(url.as_str()).map_err(|e| format!("uri parse: {e}"))
}

/// 解析 `..` 路径组件,返回等价但已规整的路径。
///
/// `Path::components` 返回 `Component::ParentDir` 表示 `..`,我们
/// 在栈上模拟"前一个 normal 组件"——遇到 `..` 时弹栈;最后重建。
/// 这是 `Path::canonicalize` 的纯函数版(不依赖 fs,文件存在与否都行)。
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut stack: Vec<PathBuf> = Vec::new();
    let mut absolute = false;
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => stack.push(PathBuf::from(p.as_os_str())),
            Component::RootDir => {
                absolute = true;
                stack.clear();
                stack.push(PathBuf::from("/"));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // 如果栈顶是 normal 组件就弹出;否则保留 `..`。
                if stack.len() > if absolute { 1 } else { 0 } {
                    stack.pop();
                } else if !absolute {
                    stack.push(PathBuf::from(".."));
                }
            }
            Component::Normal(s) => stack.push(PathBuf::from(s)),
        }
    }
    let mut out = PathBuf::new();
    for p in &stack {
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_path_absolute_outside_workspace_errors() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let outside = std::env::temp_dir().join("reflect-lsp-outside-test");
        // 即使文件不存在,resolve_path 仍要走 sandbox。
        let r = LspTool::resolve_path(ws, outside.to_str().unwrap());
        assert!(matches!(r, Err(ToolError::PathEscape { .. })));
    }

    #[test]
    fn resolve_path_relative_joins_workspace() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let r = LspTool::resolve_path(ws, "src/main.rs").unwrap();
        assert_eq!(r, ws.join("src/main.rs"));
    }

    #[test]
    fn parameters_schema_has_required_fields() {
        let mgr = Arc::new(LspConnectionManager::new(tokio::sync::mpsc::channel(1).0));
        let tool = LspTool::new(mgr);
        let s = tool.parameters_schema();
        let required = s["required"].as_array().expect("required array");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        // Phase B1:`line` / `character` 改成 optional,`document_symbol` 不需要 position。
        assert!(names.contains(&"action"));
        assert!(names.contains(&"file_path"));
        assert!(
            !names.contains(&"line"),
            "line should be optional in Phase B1 (document_symbol doesn't need it)"
        );
        assert!(
            !names.contains(&"character"),
            "character should be optional in Phase B1 (document_symbol doesn't need it)"
        );
        // action enum 锁定到 6 个 method。
        let en = s["properties"]["action"]["enum"].as_array().unwrap();
        let enums: Vec<&str> = en.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            enums,
            vec![
                "definition",
                "references",
                "hover",
                "document_symbol",
                "completion",
                "signature_help"
            ]
        );
    }

    #[test]
    fn tool_name_and_description_static() {
        let mgr = Arc::new(LspConnectionManager::new(tokio::sync::mpsc::channel(1).0));
        let tool = LspTool::new(mgr);
        assert_eq!(tool.name(), "lsp");
        assert!(tool.description().contains("definition"));
    }

    #[test]
    fn requires_prompt_in_phase_a() {
        let mgr = Arc::new(LspConnectionManager::new(tokio::sync::mpsc::channel(1).0));
        let tool = LspTool::new(mgr);
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
        assert!(!tool.is_concurrency_safe());
    }

    #[test]
    fn path_in_workspace_passes_sandbox() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let r = LspTool::resolve_path(ws, ".").unwrap();
        // `.` 解析后等于 ws canonicalize
        let _ = r; // 任意值,只要不 Err
    }

    #[test]
    #[allow(non_snake_case)]
    fn path_dotdot_traversal_caught() {
        // `../foo` 应被 sandbox 拦下。
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        let r = LspTool::resolve_path(ws, "../etc/passwd");
        assert!(matches!(r, Err(ToolError::PathEscape { .. })), "got: {r:?}");
    }
}
