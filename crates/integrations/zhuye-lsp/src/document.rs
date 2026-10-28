//! 文档状态同步。
//!
//! LSP server 拿到的方法(`textDocument/definition` 等)需要
//! `TextDocumentIdentifier` 指向已 `didOpen` 的文档。本 crate 用
//! **懒同步** 策略:调用任意 method 前若 `LspClientInner::docs` 没该 URI,
//! 读盘 → 发 `didOpen` → 记录 version+text。
//!
//! ## 为什么不主动同步全部 open files
//!
//! - 工作区可能有几万文件,主动 `didOpen` 全部既慢又吃内存;
//! - LLM 通常只问一个文件,按需打开足够;
//! - review 修复:文件被外部(Write/Edit 工具)修改后,`ensure_open`
//!   此前因 fast path `contains_key` 直接返回,LSP server 永远看到旧文本。
//!   现记录 didOpen 时的 mtime,变化时走 `didChange`(version+1,全量
//!   sync)刷新 server 视图。
//!
//! ## 版本号
//!
//! LSP `didOpen` / `didChange` 都用单调递增 `version` 字段。didOpen 时
//! version = 1;检测到磁盘变化走 didChange 每次 +1。

use std::path::Path;

use lsp_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, TextDocumentItem, Uri,
};
use tokio::fs;

use crate::client::LspClientInner;
use crate::error::LspError;

/// 单个 LSP server 视角下,一个文档的状态。
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// LSP version:didOpen 用 1,后续 didChange 单调递增。
    pub version: i32,
    /// LSP `TextDocumentItem.languageId`,告诉 server 怎么解析。
    pub language_id: String,
    /// 文档内容(didOpen / 最近一次 didChange 时的快照)。
    pub text: String,
    /// didOpen / 最近一次同步时的文件 mtime(检测外部修改)。
    pub synced_mtime: Option<std::time::SystemTime>,
}

impl LspClientInner {
    /// 调用 method 前确保 server 拿到该文档(没拿到就读盘 + didOpen)。
    ///
    /// `path` 是绝对路径(已跑过 sandbox 校验);`lang_id` 是 routing
    /// 阶段选出来的 `CompiledFilePattern.language_id`。
    ///
    /// 错误路径:`fs::read_to_string` 失败 → `LspError::Io`;`didOpen`
    /// 失败 → `LspError::Call`。
    pub async fn ensure_open(&self, path: &Path, uri: &Uri, lang_id: &str) -> Result<(), LspError> {
        // 快路径:已 open 且 mtime 未变 → 直接返回(短锁拷贝,不跨 await)。
        let synced_mtime: Option<std::time::SystemTime> =
            self.docs.read().get(uri).and_then(|st| st.synced_mtime);
        if let Some(recorded) = synced_mtime {
            let mtime = fs::metadata(path)
                .await
                .ok()
                .and_then(|m| m.modified().ok());
            if mtime == Some(recorded) {
                return Ok(());
            }
        }
        // 读盘。文件不存在(中间被删除)按 IO 错误处理,call 路径会传 ToolError::Io。
        let meta = fs::metadata(path).await.map_err(LspError::Spawn)?;
        let mtime = meta.modified().ok();
        let text = fs::read_to_string(path).await.map_err(LspError::Spawn)?;
        // 已 open 但磁盘已变 → didChange 全量同步;首次 → didOpen。
        // (先在短语句里 clone 状态,guard 不能活到 await。)
        let existing: Option<DocumentState> = self.docs.read().get(uri).cloned();
        if let Some(st) = existing
            && st.synced_mtime != mtime
        {
            let new_version = st.version + 1;
            use lsp_types::notification::DidChangeTextDocument;
            self.peer
                .notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                    text_document: lsp_types::VersionedTextDocumentIdentifier {
                        uri: uri.clone(),
                        version: new_version,
                    },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: text.clone(),
                    }],
                })
                .await?;
            self.docs.write().insert(
                uri.clone(),
                DocumentState {
                    version: new_version,
                    language_id: lang_id.to_string(),
                    text,
                    synced_mtime: mtime,
                },
            );
            return Ok(());
        }
        if self.docs.read().contains_key(uri) {
            // 已 open 且 mtime 读取不到差异(如 mtime 不可用),避免重复 didOpen。
            return Ok(());
        }
        let item = TextDocumentItem {
            uri: uri.clone(),
            language_id: lang_id.to_string(),
            // LSP 要求 version ≥ 0,初始 1。
            version: 1,
            text: text.clone(),
        };
        use lsp_types::notification::DidOpenTextDocument;
        self.peer
            .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: item,
            })
            .await?;
        // 记入 map。
        self.docs.write().insert(
            uri.clone(),
            DocumentState {
                version: 1,
                language_id: lang_id.to_string(),
                text,
                synced_mtime: mtime,
            },
        );
        Ok(())
    }

    /// `textDocument/didClose` 全部已 open 文档。`LspConnectionManager::stop_server`
    /// 关闭 server 前调用,server 收到 close 后释放 AST 内存。
    ///
    /// Best-effort:个别 `didClose` 失败不阻塞 shutdown(`tracing::warn` 即可)。
    pub async fn close_all(&self) {
        use lsp_types::notification::DidCloseTextDocument;
        let to_close: Vec<Uri> = self.docs.read().keys().cloned().collect();
        for uri in to_close {
            let params = DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            };
            if let Err(e) = self.peer.notify::<DidCloseTextDocument>(params).await {
                tracing::warn!(server = %self.server_name, ?uri, error = %e, "didClose failed");
            }
        }
        self.docs.write().clear();
    }
}

// ── `lsp_types` 重新导出避免 caller 直接 import lsp_types ─────────────

use lsp_types::{DidCloseTextDocumentParams, DidOpenTextDocumentParams};

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    use crate::client::LspClientInner;

    /// 构造一个不连 child 的 `LspClientInner`,只测文档 map 行为。
    fn make_inner() -> LspClientInner {
        LspClientInner {
            // transport.rs 公开的 `LspPeer` 字段未在 client.rs 暴露,
            // 这里用 CancellationToken + 默认 ServerCapabilities 占位。
            peer: Arc::new(crate::transport::LspPeer::stub_for_tests()),
            server_name: "test".to_string(),
            server_info: None,
            capabilities: lsp_types::ServerCapabilities::default(),
            cancel: CancellationToken::new(),
            docs: RwLock::new(HashMap::new()),
            timeout: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn close_all_is_empty_when_no_docs() {
        let inner = make_inner();
        inner.close_all().await;
        assert!(inner.docs.read().is_empty());
    }

    #[test]
    fn document_state_carries_language_id() {
        let st = DocumentState {
            version: 1,
            language_id: "rust".to_string(),
            text: "fn foo() {}".to_string(),
            synced_mtime: None,
        };
        assert_eq!(st.language_id, "rust");
        assert_eq!(st.version, 1);
    }

    #[test]
    fn uri_path_round_trip_uses_file_scheme() {
        use std::str::FromStr;
        // Windows 上 from_file_path 要求带盘符的绝对路径,不能用 /tmp。
        let path = std::env::temp_dir().join("zhuye-lsp-uri-roundtrip.rs");
        let url = url::Url::from_file_path(path).expect("from_file_path");
        let uri = lsp_types::Uri::from_str(url.as_str()).expect("uri parse");
        // lsp_types 0.97 用 fluent_uri::Scheme(无 PartialEq<str>),转字符串比对。
        let scheme = uri.scheme().map(|s| s.to_string());
        assert_eq!(scheme.as_deref(), Some("file"));
    }
}
