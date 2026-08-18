//! 通知 / 遥测 / 分析 配置 section。
//!
//! 包含:
//! - `NotificationsSection` / `TuiNotificationsSection` / `NotificationChannel`
//! - `AnalyticsSection` — OTLP 遥测
//! - `TelemetrySection` — 本地 Langfuse 式日志

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// ── 通知 ──────────────────────────────────────────────────────

/// P2: 通知集成配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct NotificationsSection {
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub tui: Option<TuiNotificationsSection>,
}

/// TUI 终端通知配置(`[notifications.tui]` 段)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TuiNotificationsSection {
    #[serde(default = "default_tui_notif_enabled")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub on_turn_complete: bool,
    #[serde(default = "default_true")]
    pub on_approval: bool,
    #[serde(default = "default_true")]
    pub on_error: bool,
    #[serde(default = "default_tui_notif_channels")]
    pub channels: Vec<String>,
}

fn default_tui_notif_enabled() -> bool {
    true
}
fn default_true() -> bool {
    true
}
fn default_tui_notif_channels() -> Vec<String> {
    vec!["bel".to_string()]
}

impl Default for TuiNotificationsSection {
    fn default() -> Self {
        Self {
            enabled: default_tui_notif_enabled(),
            on_turn_complete: default_true(),
            on_approval: default_true(),
            on_error: default_true(),
            channels: default_tui_notif_channels(),
        }
    }
}

impl TuiNotificationsSection {
    /// 解析渠道列表为有序、去重的 `NotificationChannel` 集合。
    pub fn resolved_channels(&self) -> Vec<NotificationChannel> {
        use std::collections::HashSet;
        let mut out: Vec<NotificationChannel> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for ch in &self.channels {
            let lower = ch.to_ascii_lowercase();
            if !seen.insert(seen_key(&lower)) {
                continue;
            }
            match lower.as_str() {
                "bel" | "bell" => out.push(NotificationChannel::Bell),
                "osc9" | "osc-9" => out.push(NotificationChannel::Osc9),
                "desktop" | "system" => out.push(NotificationChannel::Desktop),
                _ => {}
            }
        }
        out
    }
}

fn seen_key(lower: &str) -> &'static str {
    match lower {
        "bel" | "bell" => "bel",
        "osc9" | "osc-9" => "osc9",
        "desktop" | "system" => "desktop",
        _ => "",
    }
}

/// 已解析的通知渠道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationChannel {
    Bell,
    Osc9,
    Desktop,
}

// ── 分析遥测 ──────────────────────────────────────────────────

/// v1.3: 分析遥测(OTLP)配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnalyticsSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub service_name: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub flush_timeout_ms: Option<u64>,
}

// ── 本地 Langfuse 式日志 ──────────────────────────────────────

/// v1.2 P1:本地 Langfuse 式日志(`[telemetry]` 段)schema 镜像。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TelemetrySection {
    pub enabled: Option<bool>,
    pub dir: Option<PathBuf>,
    pub redact: Option<bool>,
    pub max_bytes: Option<u64>,
}

impl TelemetrySection {
    /// 解析最终生效的 `enabled` 值(`None` → `true`)。
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
    /// 解析最终生效的 `redact` 值(`None` → `true`)。
    pub fn should_redact(&self) -> bool {
        self.redact.unwrap_or(true)
    }
}
