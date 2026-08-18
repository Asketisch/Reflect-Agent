//! 通知集成 —— Webhook stub(Telegram/Discord/Slack/通用 URL)。

use serde::{Deserialize, Serialize};

/// 支持的通知渠道(Phase stub 仅记录,不真正投递除非启用 `webhook` feature)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationChannel {
    Webhook,
    Telegram,
    Discord,
    Slack,
    System,
}

/// 一条待发送通知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPayload {
    pub channel: NotificationChannel,
    pub title: String,
    pub body: String,
    /// 可选 webhook URL(从 config 注入)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
}

/// Webhook 通知器 stub。
///
/// 默认仅 `tracing::info!` 记录;启用 `reflect-integration/webhook` feature
/// 且提供 URL 时会 POST JSON body。
#[derive(Debug, Clone, Default)]
pub struct WebhookNotifier {
    pub default_url: Option<String>,
}

impl WebhookNotifier {
    pub fn new(default_url: Option<String>) -> Self {
        Self { default_url }
    }

    /// 发送通知(best-effort,失败不 panic)。
    pub async fn send(&self, payload: NotificationPayload) -> anyhow::Result<()> {
        let url = payload
            .webhook_url
            .clone()
            .or_else(|| self.default_url.clone());
        tracing::info!(
            channel = ?payload.channel,
            title = %payload.title,
            has_url = url.is_some(),
            "notifications stub: 记录通知(真实投递留 v2.x)"
        );
        #[cfg(feature = "webhook")]
        if let Some(u) = url {
            let body = serde_json::json!({
                "title": payload.title,
                "body": payload.body,
                "source": "reflect",
            });
            let client = reqwest::Client::new();
            let resp = client.post(&u).json(&body).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("webhook HTTP {}", resp.status());
            }
            return Ok(());
        }
        Ok(())
    }

    /// TUI / CLI 状态摘要。
    pub fn status_line(&self) -> String {
        match &self.default_url {
            Some(u) if !u.is_empty() => format!("notifications: webhook 已配置 ({})", mask_url(u)),
            _ => "notifications: stub(未配置 webhook_url — 见 [notifications] 段)".into(),
        }
    }
}

fn mask_url(url: &str) -> String {
    if url.len() <= 12 {
        return "***".into();
    }
    format!("{}…{}", &url[..8], &url[url.len().saturating_sub(4)..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_without_url_is_ok() {
        let n = WebhookNotifier::default();
        n.send(NotificationPayload {
            channel: NotificationChannel::Webhook,
            title: "t".into(),
            body: "b".into(),
            webhook_url: None,
        })
        .await
        .unwrap();
    }

    #[test]
    fn status_line_stub() {
        let n = WebhookNotifier::default();
        assert!(n.status_line().contains("stub"));
    }
}
