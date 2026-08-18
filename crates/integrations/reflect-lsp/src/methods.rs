//! LSP `action` 枚举 + 各 action 的强类型 invoke 包装。
//!
//! `LspAction` 是 `LspTool::execute` 收到的 `action` 字段反序列化目标。
//! 每个 variant 对应一个 LSP `textDocument/*` request,`invoke_*` 包装
//! `LspClientInner::peer.request::<R>(params)` 调用,把超时 + cancel
//! 都集中在这里。
//!
//! ## 范围
//!
//! **v0.5.0** — Phase A 3 个只读 method + Phase B1 再加 3 个:
//!
//! - `textDocument/definition` (`lsp_types::request::GotoDefinition`)
//! - `textDocument/references` (`lsp_types::request::References`)
//! - `textDocument/hover`     (`lsp_types::request::HoverRequest`)
//! - `textDocument/documentSymbol` (`lsp_types::request::DocumentSymbolRequest`)
//! - `textDocument/completion`     (`lsp_types::request::Completion`)
//! - `textDocument/signatureHelp`  (`lsp_types::request::SignatureHelpRequest`)
//!
//! 仍留 Phase B 后续:`diagnostic` (pull 模式) / `rename` / `codeAction`。
//! 后两者会动 `WorkspaceEdit` 落盘,是 Phase B 的大头。

use std::time::Instant;

use lsp_types::{
    CompletionParams, CompletionResponse, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, Position, ReferenceContext,
    ReferenceParams, SignatureHelp, SignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri,
    request::{
        Completion, DocumentSymbolRequest, GotoDefinition, HoverRequest, References,
        SignatureHelpRequest,
    },
};
use serde::Deserialize;
use tokio::time::timeout;

use reflect_protocol::PermissionMode;

use crate::client::LspClientInner;
use crate::error::LspError;

/// LLM 在 `lsp` tool 调用里填的 `action` 字段。
///
/// Phase A 3 个 variant + Phase B1 3 个,Phase B 后续再扩 mutation method。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspAction {
    /// 跳到 symbol 定义位置。LSP: `textDocument/definition`。
    Definition,
    /// 找所有引用位置。LSP: `textDocument/references`。
    References,
    /// 类型 / 文档 hover。LSP: `textDocument/hover`。
    Hover,
    /// 文件大纲(函数 / 类 / 变量)。LSP: `textDocument/documentSymbol`。
    DocumentSymbol,
    /// 上下文补全。LSP: `textDocument/completion`。
    Completion,
    /// 函数签名提示。LSP: `textDocument/signatureHelp`。
    SignatureHelp,
}

impl LspAction {
    /// 当前 6 个 action 全部只读。`rename` / `codeAction` 留 Phase B 后续。
    pub fn is_read_only(self) -> bool {
        true
    }

    /// Per-action 权限表(Phase B1):
    ///
    /// 只读 action 全部 `Auto` —— 改善 Phase A "hover 都要 confirm" 的 UX
    /// 摩擦(用户在 Phase A 测试时反复反馈)。`rename` / `codeAction` 留
    /// Phase B 后续,届时新增 mutation variant → `Prompt`。
    ///
    /// **注意**:`LspTool::required_permission()` 的 tool-level fallback
    /// 是 `Auto`,但 `execute` 内部按 action 走 per-action 表,所以这个
    /// 表生效(优先于 tool-level)。
    pub fn required_permission(self) -> PermissionMode {
        match self {
            LspAction::Definition
            | LspAction::References
            | LspAction::Hover
            | LspAction::DocumentSymbol
            | LspAction::Completion
            | LspAction::SignatureHelp => PermissionMode::Auto,
        }
    }

    /// Per-action 并发安全标记。Phase B1 6 个全 `true`(只读 + 各自独立
    /// 的 server peer),让 LLM 可以并发触发 5 个 lsp 调用提升效率。
    pub fn is_concurrency_safe(self) -> bool {
        match self {
            LspAction::Definition
            | LspAction::References
            | LspAction::Hover
            | LspAction::DocumentSymbol
            | LspAction::Completion
            | LspAction::SignatureHelp => true,
        }
    }

    /// LSP method 字符串(给 `LspLifecycleEvent::Started.methods` 字段用)。
    pub fn method_str(self) -> &'static str {
        match self {
            LspAction::Definition => "textDocument/definition",
            LspAction::References => "textDocument/references",
            LspAction::Hover => "textDocument/hover",
            LspAction::DocumentSymbol => "textDocument/documentSymbol",
            LspAction::Completion => "textDocument/completion",
            LspAction::SignatureHelp => "textDocument/signatureHelp",
        }
    }

    /// 支持该 action 的 LSP `ServerCapabilities` 字段名。
    ///
    /// `LspServerHandle::supported_methods` 用这个推断 `ServerCapabilities`
    /// 里哪个 flag 决定 server 是否支持该 method。
    pub fn capability_field(self) -> Option<&'static str> {
        match self {
            LspAction::Definition => Some("definition_provider"),
            LspAction::References => Some("references_provider"),
            LspAction::Hover => Some("hover_provider"),
            LspAction::DocumentSymbol => Some("document_symbol_provider"),
            LspAction::Completion => Some("completion_provider"),
            LspAction::SignatureHelp => Some("signature_help_provider"),
        }
    }
}

// ── invoke_* 包装:每条 method 一个强类型函数 ──────────────────────────

/// `textDocument/definition` —— 跳到 symbol 定义。
///
/// 返回 `GotoDefinitionResponse` 可能是单 location / location 数组 /
/// location link 数组,`lsp_types` 用 enum 涵盖。
pub async fn invoke_definition(
    client: &LspClientInner,
    uri: &Uri,
    position: Position,
) -> Result<Option<GotoDefinitionResponse>, LspError> {
    let params = GotoDefinitionParams {
        text_document_position_params: text_doc_pos(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    timed(client, async {
        client.peer.request::<GotoDefinition>(params).await
    })
    .await
}

/// `textDocument/references` —— 找所有引用。
///
/// `include_declaration` 控制是否把声明位置也放进结果(默认 `true`)。
pub async fn invoke_references(
    client: &LspClientInner,
    uri: &Uri,
    position: Position,
    include_declaration: bool,
) -> Result<Vec<lsp_types::Location>, LspError> {
    let params = ReferenceParams {
        text_document_position: text_doc_pos(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration,
        },
    };
    let result = timed(client, async {
        client.peer.request::<References>(params).await
    })
    .await?;
    // `References::Result = Option<Vec<Location>>`;`None` → 空 Vec。
    Ok(result.unwrap_or_default())
}

/// `textDocument/hover` —— 类型 / 文档 hover。
///
/// `Option<Hover>` —— `None` 表示 server 判定该位置无 hover 信息。
pub async fn invoke_hover(
    client: &LspClientInner,
    uri: &Uri,
    position: Position,
) -> Result<Option<Hover>, LspError> {
    let params = HoverParams {
        text_document_position_params: text_doc_pos(uri, position),
        work_done_progress_params: Default::default(),
    };
    timed(client, async {
        client.peer.request::<HoverRequest>(params).await
    })
    .await
}

/// `textDocument/documentSymbol` —— 文件大纲。
///
/// `DocumentSymbolResponse` 是 enum:`Flat(Vec<SymbolInformation>)` 或
/// `Nested(Vec<DocumentSymbol>)`(层级结构,Phase A 用 mock 返回 Nested)。
pub async fn invoke_document_symbol(
    client: &LspClientInner,
    uri: &Uri,
) -> Result<Option<DocumentSymbolResponse>, LspError> {
    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    timed(client, async {
        client.peer.request::<DocumentSymbolRequest>(params).await
    })
    .await
}

/// `textDocument/completion` —— 上下文补全。
///
/// `CompletionResponse` 是 enum:`Array(Vec<CompletionItem>)` 或
/// `List(CompletionList { is_incomplete, items })`。
pub async fn invoke_completion(
    client: &LspClientInner,
    uri: &Uri,
    position: Position,
) -> Result<Option<CompletionResponse>, LspError> {
    let params = CompletionParams {
        text_document_position: text_doc_pos(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    timed(client, async {
        client.peer.request::<Completion>(params).await
    })
    .await
}

/// `textDocument/signatureHelp` —— 函数签名提示。
///
/// `Option<SignatureHelp>` —— `None` 表示 server 判定该位置无签名信息。
pub async fn invoke_signature_help(
    client: &LspClientInner,
    uri: &Uri,
    position: Position,
) -> Result<Option<SignatureHelp>, LspError> {
    let params = SignatureHelpParams {
        text_document_position_params: text_doc_pos(uri, position),
        work_done_progress_params: Default::default(),
        context: None,
    };
    timed(client, async {
        client.peer.request::<SignatureHelpRequest>(params).await
    })
    .await
}

// ── 私有 helper ──────────────────────────────────────────────────────

/// 构造 `TextDocumentPositionParams`(LSP 大多数 position 调用的基础结构)。
fn text_doc_pos(uri: &Uri, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    }
}

/// 给一个 future 加 timeout + cancel 双层保护。`LspServerConfig.timeout`
/// 给单次 request 的硬上限;`LspClientInner::cancel` 是 manager shutdown
/// 触发的全局 cancel。
async fn timed<T, F>(client: &LspClientInner, fut: F) -> Result<T, LspError>
where
    F: std::future::Future<Output = Result<T, LspError>>,
{
    let started = Instant::now();
    let to = client.timeout;
    tokio::select! {
        biased;
        _ = client.cancel.cancelled() => Err(LspError::Cancelled),
        r = timeout(to, fut) => {
            match r {
                Ok(inner) => inner,
                Err(_) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    // timeout 后用 `Duration` 默认 30s 报即可,与 `LspError::Timeout` 形态一致。
                    let _ = elapsed;
                    Err(LspError::Timeout {
                        elapsed_ms: to.as_millis() as u64,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serde_roundtrip_all_variants() {
        for (raw, want) in [
            (r#""definition""#, LspAction::Definition),
            (r#""references""#, LspAction::References),
            (r#""hover""#, LspAction::Hover),
            (r#""document_symbol""#, LspAction::DocumentSymbol),
            (r#""completion""#, LspAction::Completion),
            (r#""signature_help""#, LspAction::SignatureHelp),
        ] {
            let parsed: LspAction = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, want);
        }
    }

    #[test]
    fn invalid_action_errors() {
        let r: Result<LspAction, _> = serde_json::from_str(r#""rename""#);
        assert!(r.is_err());
    }

    #[test]
    fn all_actions_are_read_only() {
        for action in [
            LspAction::Definition,
            LspAction::References,
            LspAction::Hover,
            LspAction::DocumentSymbol,
            LspAction::Completion,
            LspAction::SignatureHelp,
        ] {
            assert!(action.is_read_only(), "{action:?} not read-only");
            // Phase B1 全部 Auto,UX 改善:用户不需要为 hover 反复 confirm。
            assert_eq!(action.required_permission(), PermissionMode::Auto);
            // Phase B1 全部并发安全。
            assert!(action.is_concurrency_safe(), "{action:?} not concurrent");
        }
    }

    #[test]
    fn method_str_matches_lsp_spec() {
        assert_eq!(
            LspAction::Definition.method_str(),
            "textDocument/definition"
        );
        assert_eq!(
            LspAction::References.method_str(),
            "textDocument/references"
        );
        assert_eq!(LspAction::Hover.method_str(), "textDocument/hover");
        assert_eq!(
            LspAction::DocumentSymbol.method_str(),
            "textDocument/documentSymbol"
        );
        assert_eq!(
            LspAction::Completion.method_str(),
            "textDocument/completion"
        );
        assert_eq!(
            LspAction::SignatureHelp.method_str(),
            "textDocument/signatureHelp"
        );
    }

    #[test]
    fn capability_field_covers_all_actions() {
        // 6 个 action 都对应一个 `ServerCapabilities` 字段。
        for action in [
            LspAction::Definition,
            LspAction::References,
            LspAction::Hover,
            LspAction::DocumentSymbol,
            LspAction::Completion,
            LspAction::SignatureHelp,
        ] {
            assert!(action.capability_field().is_some(), "{action:?} missing");
        }
    }
}
