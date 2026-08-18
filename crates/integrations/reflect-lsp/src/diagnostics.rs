//! LSP 诊断注册表 —— Phase B:缓存 `textDocument/publishDiagnostics` 通知。
//!
//! server 推送的诊断经 `DiagnosticRegistry::ingest_notification` 写入,
//! `LspTool` / TUI 可被动读取最新诊断而不主动 pull。

use std::collections::HashMap;
use std::sync::Arc;

use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};
use parking_lot::RwLock;
use serde_json::Value;

/// 按文档 URI 缓存的最新诊断列表。
#[derive(Debug, Default, Clone)]
pub struct DiagnosticRegistry {
    inner: Arc<RwLock<HashMap<String, Vec<Diagnostic>>>>,
}

impl DiagnosticRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理 LSP notification JSON( method 应为 publishDiagnostics )。
    pub fn ingest_notification(&self, method: &str, params: &Value) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params.clone()) else {
            tracing::warn!("diagnostics: 无法解析 publishDiagnostics params");
            return;
        };
        self.set(p.uri, p.diagnostics);
    }

    /// 写入/覆盖某 URI 的诊断。
    pub fn set(&self, uri: Uri, diagnostics: Vec<Diagnostic>) {
        let key = uri.to_string();
        self.inner.write().insert(key, diagnostics);
    }

    /// 读取某 URI 的诊断(无则空 Vec)。
    pub fn get(&self, uri: &Uri) -> Vec<Diagnostic> {
        let key = uri.to_string();
        self.inner.read().get(&key).cloned().unwrap_or_default()
    }

    /// 所有已缓存 URI 数量(被动反馈指标)。
    pub fn document_count(&self) -> usize {
        self.inner.read().len()
    }

    /// 诊断总数(跨文档)。
    pub fn diagnostic_count(&self) -> usize {
        self.inner.read().values().map(|v| v.len()).sum()
    }

    /// TUI / CLI 摘要。
    pub fn status_line(&self) -> String {
        format!(
            "lsp diagnostics: {} 个文档, {} 条诊断(被动缓存)",
            self.document_count(),
            self.diagnostic_count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_and_get() {
        let reg = DiagnosticRegistry::new();
        let uri: Uri = "file:///tmp/a.rs".parse().unwrap();
        let params = serde_json::json!({
            "uri": uri,
            "diagnostics": [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                },
                "message": "test",
                "severity": 1
            }]
        });
        reg.ingest_notification("textDocument/publishDiagnostics", &params);
        assert_eq!(reg.get(&uri).len(), 1);
        assert_eq!(reg.diagnostic_count(), 1);
    }

    #[test]
    fn ignores_other_methods() {
        let reg = DiagnosticRegistry::new();
        reg.ingest_notification("window/logMessage", &serde_json::json!({}));
        assert_eq!(reg.document_count(), 0);
    }
}
