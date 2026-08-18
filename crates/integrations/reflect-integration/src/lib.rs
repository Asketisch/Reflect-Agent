//! `reflect-integration` — 外部集成层 stub 与安全扫描。
//!
//! 本 crate 聚合 P2 集成特性:
//! - `security_scan`: cargo audit CLI 包装
//! - `notifications`: Webhook 通知 stub

pub mod notifications;
pub mod security_scan;

pub use notifications::{NotificationChannel, NotificationPayload, WebhookNotifier};
pub use security_scan::{SecurityScanReport, run_cargo_audit};
