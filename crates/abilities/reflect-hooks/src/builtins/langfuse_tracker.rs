//! `langfuse_tracker` — SessionStart / Stop 事件导出到 tracing 或 Langfuse HTTP/OTLP。

use async_trait::async_trait;
use tracing::{info, warn};

use crate::config::LangfuseConfig;
use crate::decision::HookDecision;
use crate::event::{HookEvent, HookEventKind, StopReason};
use crate::hook::Hook;

/// 根据配置把 session 生命周期事件上报 Langfuse(或 OTLP stub)。
pub struct LangfuseTracker {
    cfg: LangfuseConfig,
}

impl LangfuseTracker {
    pub fn new(cfg: LangfuseConfig) -> Self {
        Self { cfg }
    }

    fn export_mode(&self) -> &str {
        self.cfg.export_mode.as_deref().unwrap_or("tracing")
    }

    async fn export_session_start(&self, session_id: &str, model: &str) {
        info!(target: "langfuse", session_id = %session_id, model = %model, "session_start");
        match self.export_mode() {
            "http" => {
                self.export_http("session_start", session_id, model, None)
                    .await
            }
            "otlp" => self.export_otlp_stub("session_start", session_id, model),
            _ => {}
        }
    }

    async fn export_stop(&self, reason: &StopReason, attempt: u32) {
        info!(target: "langfuse", reason = ?reason, attempt, "stop");
        match self.export_mode() {
            "http" => {
                self.export_http("stop", "", "unknown", Some(format!("{reason:?}")))
                    .await
            }
            "otlp" => self.export_otlp_stub("stop", "", &format!("{reason:?}")),
            _ => {}
        }
    }

    async fn export_http(
        &self,
        event_type: &str,
        session_id: &str,
        model: &str,
        extra: Option<String>,
    ) {
        let Some(endpoint) = self.cfg.endpoint.as_deref() else {
            warn!(target: "langfuse", "export_mode=http but endpoint unset; falling back to tracing only");
            return;
        };
        let pk = self.cfg.public_key.as_deref().unwrap_or("");
        let sk = self.cfg.secret_key.as_deref().unwrap_or("");
        if pk.is_empty() || sk.is_empty() {
            warn!(target: "langfuse", "export_mode=http but public_key/secret_key missing");
            return;
        }
        let url = format!("{}/api/public/ingestion", endpoint.trim_end_matches('/'));
        let batch = serde_json::json!({
            "batch": [{
                "type": event_type,
                "body": {
                    "sessionId": session_id,
                    "metadata": {
                        "model": model,
                        "extra": extra,
                    }
                }
            }]
        });
        let client = reqwest::Client::new();
        match client
            .post(&url)
            .basic_auth(pk, Some(sk))
            .json(&batch)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(target: "langfuse", status = %resp.status(), "http export ok");
            }
            Ok(resp) => {
                warn!(target: "langfuse", status = %resp.status(), "langfuse http export failed");
            }
            Err(e) => {
                warn!(target: "langfuse", error = %e, "langfuse http export error");
            }
        }
    }

    fn export_otlp_stub(&self, event_type: &str, session_id: &str, detail: &str) {
        let endpoint = self
            .cfg
            .endpoint
            .as_deref()
            .unwrap_or("http://localhost:4318/v1/traces");
        info!(
            target: "langfuse",
            otlp_endpoint = %endpoint,
            event = %event_type,
            session_id = %session_id,
            detail = %detail,
            "otlp export stub (not wired; use export_mode=http for Langfuse REST)"
        );
    }
}

impl Default for LangfuseTracker {
    fn default() -> Self {
        Self::new(LangfuseConfig::default())
    }
}

#[async_trait]
impl Hook for LangfuseTracker {
    fn name(&self) -> &str {
        "langfuse_tracker"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::SessionStart, HookEventKind::Stop]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, crate::hook::HookError> {
        match event {
            HookEvent::SessionStart { session_id, config } => {
                let model = config
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                self.export_session_start(&session_id.to_string(), model)
                    .await;
            }
            HookEvent::Stop { reason, attempt } => {
                self.export_stop(reason, *attempt).await;
            }
            _ => {}
        }
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_start_does_not_error() {
        let h = LangfuseTracker::default();
        let e = HookEvent::SessionStart {
            session_id: reflect_protocol::ThreadId::new(),
            config: serde_json::json!({"model": "openai/gpt-4o"}),
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn stop_does_not_error() {
        let h = LangfuseTracker::default();
        let e = HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: 0,
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn otlp_stub_mode_logs_without_http() {
        let h = LangfuseTracker::new(LangfuseConfig {
            export_mode: Some("otlp".into()),
            endpoint: Some("http://localhost:4318".into()),
            ..Default::default()
        });
        let e = HookEvent::SessionStart {
            session_id: reflect_protocol::ThreadId::new(),
            config: serde_json::json!({}),
        };
        assert_eq!(h.handle(&e).await.unwrap(), HookDecision::Allow);
    }
}
