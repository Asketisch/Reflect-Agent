//! v1.x 功能 7:厂商 quota API 适配器。
//!
//! 参考 cc-switch(`src-tauri/src/services/coding_plan.rs`)的国产 Token Plan
//! 额度查询实现。支持 Kimi / 智谱 GLM / MiniMax / ZenMux 四家,均走 OpenAI
//! 兼容协议(配在 `[openai]` provider 下,靠 credential `base_url` 区分)。
//!
//! 各厂商的 quota 查询 endpoint / 鉴权 / 响应格式不同,但归一化到统一的
//! [`QuotaSnapshot`] —— 包含剩余 token 数与窗口重置时间。`QuotaTracker`
//! 据此判定 credential 是否耗尽(替代纯本地累计)。
//!
//! ## 设计要点(对齐 cc-switch)
//!
//! - **瞬时 vs 确定性失败**:网络/超时 → `Err`(调用方回退本地统计);
//!   鉴权/解析失败 → `Ok(snapshot.success=false)`(确定性,credential 不可用)。
//! - **读体再解析**:先 `bytes()` 读完整 body(读体失败 = 瞬时),再
//!   `serde_json::from_slice`(解析失败 = 确定性)。
//! - **数字/字符串兼容**:`parse_f64` 兼容 `"100"` 与 `100`。
//! - **重置时间兼容**:`extract_reset_time` 兼容 ISO 字符串 / 秒 / 毫秒。

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use thiserror::Error;

/// 厂商 quota 查询统一结果。所有适配器归一化到此结构。
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    /// 查询是否成功。`false` 表示鉴权/解析等确定性失败(credential 不可用)。
    pub success: bool,
    /// 错误描述(`success=false` 时)。
    pub error: Option<String>,
    /// 主窗口(通常是 5 小时)的已用百分比 0-100。`None` = 厂商未返回。
    /// `>= 100.0` 即视为耗尽。
    pub utilization: Option<f64>,
    /// 窗口重置时间(UTC)。`None` = 厂商未返回。
    pub resets_at: Option<DateTime<Utc>>,
    /// 厂商返回的剩余 token 绝对值(部分厂商直接给 remaining 而非百分比)。
    /// 优先用此判定耗尽;`None` 时回退 `utilization`。
    pub remaining_tokens: Option<u64>,
    /// 厂商返回的窗口上限 token 绝对值(部分厂商给 limit + remaining)。
    pub max_tokens: Option<u64>,
}

impl QuotaSnapshot {
    /// 该窗口是否已耗尽。优先用 `remaining_tokens == 0`,否则 `utilization >= 100`。
    pub fn is_exhausted(&self) -> bool {
        if !self.success {
            return false; // 查询失败不判耗尽,交给本地统计兜底。
        }
        if let Some(rem) = self.remaining_tokens {
            return rem == 0;
        }
        self.utilization.is_some_and(|u| u >= 100.0)
    }
}

/// quota 查询错误(瞬时失败,调用方回退本地统计)。
#[derive(Debug, Clone, Error)]
pub enum QuotaError {
    #[error("network error: {0}")]
    Network(String),
    #[error("failed to read response body: {0}")]
    BodyRead(String),
}

/// 厂商 quota 适配器 trait。每个厂商实现 `query` 返回 `QuotaSnapshot`。
#[async_trait]
pub trait QuotaProvider: Send + Sync {
    /// 查询该 credential 的配额快照。
    /// `base_url` 是 credential 配置的 base_url(用于区分厂商 / 拼 endpoint)。
    /// `api_key` 是 credential 的 API key。
    async fn query(&self, base_url: &str, api_key: &str) -> Result<QuotaSnapshot, QuotaError>;
}

// ── 辅助函数(对齐 cc-switch) ────────────────────────────────────

/// 解析 JSON 值为 f64,兼容数字和字符串格式。
fn parse_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// 从 JSON 值提取重置时间,兼容 ISO 字符串 / 秒 / 毫秒。
fn extract_reset_time(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc));
    }
    if let Some(n) = v.as_i64() {
        if n <= 0 {
            return None;
        }
        let ms = if n < 1_000_000_000_000 { n * 1000 } else { n };
        return DateTime::from_timestamp_millis(ms);
    }
    None
}

/// 构造一个「确定性失败」快照(鉴权/解析错误,credential 不可用)。
fn failed_snapshot(msg: impl Into<String>) -> QuotaSnapshot {
    QuotaSnapshot {
        success: false,
        error: Some(msg.into()),
        utilization: None,
        resets_at: None,
        remaining_tokens: None,
        max_tokens: None,
    }
}

/// 构造 HTTP client(复用 reflect-llm 既有 reqwest 配置)。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 通用:发送 GET 请求,返回 (status, body_bytes)。网络/读体错误转 `QuotaError`。
async fn fetch_get(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(reqwest::StatusCode, Vec<u8>), QuotaError> {
    let client = http_client();
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| QuotaError::Network(e.to_string()))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| QuotaError::BodyRead(e.to_string()))?
        .to_vec();
    Ok((status, bytes))
}

// ── Kimi（For Coding）─────────────────────────────────────────────

/// Kimi 额度查询(`GET https://api.kimi.com/coding/v1/usages`)。
///
/// 响应:`{ limits: [{ detail: { limit, remaining, resetTime } }], usage: { limit, remaining, resetTime } }`
/// `limits` 是 5 小时窗口,`usage` 是周窗口。取 5 小时窗口判定耗尽。
pub struct KimiQuotaProvider;

#[async_trait]
impl QuotaProvider for KimiQuotaProvider {
    async fn query(&self, _base_url: &str, api_key: &str) -> Result<QuotaSnapshot, QuotaError> {
        let url = "https://api.kimi.com/coding/v1/usages";
        let auth = format!("Bearer {api_key}");
        let (status, body) = fetch_get(
            url,
            &[("Authorization", &auth), ("Accept", "application/json")],
        )
        .await?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(failed_snapshot(format!(
                "Authentication failed (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Ok(failed_snapshot(format!("API error (HTTP {status})")));
        }
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return Ok(failed_snapshot(format!("Failed to parse response: {e}"))),
        };

        // 5 小时窗口(limits[].detail)。
        if let Some(limits) = v.get("limits").and_then(|x| x.as_array()) {
            for item in limits {
                if let Some(detail) = item.get("detail") {
                    let limit =
                        parse_f64(detail.get("limit").unwrap_or(&Value::Null)).unwrap_or(0.0);
                    let remaining =
                        parse_f64(detail.get("remaining").unwrap_or(&Value::Null)).unwrap_or(0.0);
                    let resets_at = detail.get("resetTime").and_then(extract_reset_time);
                    let used = (limit - remaining).max(0.0);
                    let util = if limit > 0.0 {
                        (used / limit) * 100.0
                    } else {
                        0.0
                    };
                    return Ok(QuotaSnapshot {
                        success: true,
                        error: None,
                        utilization: Some(util),
                        resets_at,
                        remaining_tokens: Some(remaining as u64),
                        max_tokens: Some(limit as u64),
                    });
                }
            }
        }
        Ok(failed_snapshot("No 5-hour limit tier in Kimi response"))
    }
}

// ── 智谱 GLM ─────────────────────────────────────────────────────

/// 智谱额度查询(`GET {base}/api/monitor/usage/quota/limit`)。
///
/// 响应:`{ success: bool, data: { limits: [{ type: "TOKENS_LIMIT", percentage, nextResetTime, unit }] } }`
/// `unit: 3` = 5 小时窗口,`unit: 6` = 周窗口。注意:鉴权用裸 api_key(无 Bearer)。
pub struct ZhipuQuotaProvider;

impl ZhipuQuotaProvider {
    /// 按 base_url 选 quota endpoint host(bigmodel.cn vs z.ai)。
    fn quota_base(base_url: &str) -> &str {
        if base_url.to_lowercase().contains("bigmodel.cn") {
            "https://open.bigmodel.cn"
        } else {
            "https://api.z.ai"
        }
    }
}

#[async_trait]
impl QuotaProvider for ZhipuQuotaProvider {
    async fn query(&self, base_url: &str, api_key: &str) -> Result<QuotaSnapshot, QuotaError> {
        let url = format!(
            "{}/api/monitor/usage/quota/limit",
            Self::quota_base(base_url)
        );
        // 智谱不加 Bearer 前缀!
        let (status, body) = fetch_get(
            &url,
            &[
                ("Authorization", api_key),
                ("Content-Type", "application/json"),
                ("Accept-Language", "en-US,en"),
            ],
        )
        .await?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(failed_snapshot(format!(
                "Authentication failed (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Ok(failed_snapshot(format!("API error (HTTP {status})")));
        }
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return Ok(failed_snapshot(format!("Failed to parse response: {e}"))),
        };
        if v.get("success").and_then(|x| x.as_bool()) == Some(false) {
            let msg = v
                .get("msg")
                .and_then(|x| x.as_str())
                .unwrap_or("Unknown error");
            return Ok(failed_snapshot(format!("API error: {msg}")));
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => return Ok(failed_snapshot("Missing 'data' field in response")),
        };

        // 取 unit=3(5 小时)窗口;若无则取第一条 TOKENS_LIMIT。
        let limits = data.get("limits").and_then(|x| x.as_array());
        if let Some(limits) = limits {
            let mut best: Option<(f64, Option<DateTime<Utc>>)> = None;
            for item in limits {
                let ltype = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if !ltype.eq_ignore_ascii_case("TOKENS_LIMIT") {
                    continue;
                }
                let pct = item
                    .get("percentage")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0);
                let resets = item.get("nextResetTime").and_then(extract_reset_time);
                let is_five_hour = item.get("unit").and_then(|x| x.as_i64()) == Some(3);
                if is_five_hour {
                    best = Some((pct, resets));
                    break;
                }
                if best.is_none() {
                    best = Some((pct, resets));
                }
            }
            if let Some((pct, resets)) = best {
                return Ok(QuotaSnapshot {
                    success: true,
                    error: None,
                    utilization: Some(pct),
                    resets_at: resets,
                    remaining_tokens: None, // 智谱只给百分比,无绝对值
                    max_tokens: None,
                });
            }
        }
        Ok(failed_snapshot("No TOKENS_LIMIT tier in Zhipu response"))
    }
}

// ── MiniMax（MiniMax 国产额度）──────────────────────────────────────

/// MiniMax 额度查询(`GET https://{domain}/v1/api/openplatform/coding_plan/remains`)。
///
/// 响应:`{ model_remains: [{ model_name: "general", current_interval_remaining_percent, end_time }] }`
/// `current_interval_remaining_percent` 是**剩余**百分比,反转为已用。
pub struct MinimaxQuotaProvider;

impl MinimaxQuotaProvider {
    /// 按 base_url 选域名(cn vs 国际)。
    fn domain(base_url: &str) -> &'static str {
        let u = base_url.to_lowercase();
        if u.contains("api.minimax.io") {
            "api.minimax.io"
        } else {
            "api.minimaxi.com"
        }
    }
}

#[async_trait]
impl QuotaProvider for MinimaxQuotaProvider {
    async fn query(&self, base_url: &str, api_key: &str) -> Result<QuotaSnapshot, QuotaError> {
        let url = format!(
            "https://{}/v1/api/openplatform/coding_plan/remains",
            Self::domain(base_url)
        );
        let auth = format!("Bearer {api_key}");
        let (status, body) = fetch_get(
            &url,
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
        )
        .await?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(failed_snapshot(format!(
                "Authentication failed (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Ok(failed_snapshot(format!("API error (HTTP {status})")));
        }
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return Ok(failed_snapshot(format!("Failed to parse response: {e}"))),
        };
        // 业务级错误。
        if let Some(base_resp) = v.get("base_resp") {
            let code = base_resp
                .get("status_code")
                .and_then(|x| x.as_i64())
                .unwrap_or(-1);
            if code != 0 {
                let msg = base_resp
                    .get("status_msg")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Unknown error");
                return Ok(failed_snapshot(format!("API error (code {code}): {msg}")));
            }
        }

        let model_remains = v.get("model_remains").and_then(|x| x.as_array());
        if let Some(arr) = model_remains {
            // 只取 model_name == "general"。
            for item in arr {
                let name = item
                    .get("model_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if name != "general" {
                    continue;
                }
                if let Some(remain_pct) = item
                    .get("current_interval_remaining_percent")
                    .and_then(|x| x.as_f64())
                {
                    let resets_at = item.get("end_time").and_then(extract_reset_time);
                    // remaining 是剩余百分比,反转为已用。
                    let util = 100.0 - remain_pct;
                    return Ok(QuotaSnapshot {
                        success: true,
                        error: None,
                        utilization: Some(util),
                        resets_at,
                        remaining_tokens: None,
                        max_tokens: None,
                    });
                }
            }
        }
        Ok(failed_snapshot(
            "No 'general' model_remains in MiniMax response",
        ))
    }
}

// ── ZenMux（兜底接入）──────────────────────────────────────────────

/// ZenMux 额度查询(`GET {base_url}`,直接查 base_url)。
///
/// 响应:`{ success: bool, data: { quota_5_hour: { usage_percentage, resets_at } } }`
/// `usage_percentage` 是 0-1 的小数(乘 100 转百分比)。
pub struct ZenmuxQuotaProvider;

#[async_trait]
impl QuotaProvider for ZenmuxQuotaProvider {
    async fn query(&self, base_url: &str, api_key: &str) -> Result<QuotaSnapshot, QuotaError> {
        let auth = format!("Bearer {api_key}");
        let (status, body) = fetch_get(
            base_url,
            &[("Authorization", &auth), ("Accept", "application/json")],
        )
        .await?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(failed_snapshot(format!(
                "Authentication failed (HTTP {status})"
            )));
        }
        if !status.is_success() {
            return Ok(failed_snapshot(format!("API error (HTTP {status})")));
        }
        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return Ok(failed_snapshot(format!("Failed to parse response: {e}"))),
        };
        if v.get("success").and_then(|x| x.as_bool()) != Some(true) {
            let msg = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("Unknown error");
            return Ok(failed_snapshot(format!("API error: {msg}")));
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => return Ok(failed_snapshot("Missing 'data' field in response")),
        };

        // 5 小时窗口。
        if let Some(q5h) = data.get("quota_5_hour") {
            let usage_pct =
                parse_f64(q5h.get("usage_percentage").unwrap_or(&Value::Null)).unwrap_or(0.0);
            let resets_at = q5h.get("resets_at").and_then(|x| x.as_str()).and_then(|s| {
                DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            });
            return Ok(QuotaSnapshot {
                success: true,
                error: None,
                utilization: Some(usage_pct * 100.0),
                resets_at,
                remaining_tokens: None,
                max_tokens: None,
            });
        }
        Ok(failed_snapshot("No quota_5_hour in ZenMux response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_exhausted_by_remaining_zero() {
        let s = QuotaSnapshot {
            success: true,
            error: None,
            utilization: Some(50.0),
            resets_at: None,
            remaining_tokens: Some(0),
            max_tokens: Some(1000),
        };
        assert!(s.is_exhausted());
    }

    #[test]
    fn snapshot_exhausted_by_utilization() {
        let s = QuotaSnapshot {
            success: true,
            error: None,
            utilization: Some(100.0),
            resets_at: None,
            remaining_tokens: None,
            max_tokens: None,
        };
        assert!(s.is_exhausted());
    }

    #[test]
    fn snapshot_not_exhausted_when_query_failed() {
        // 查询失败(success=false)不判耗尽,交本地统计兜底。
        let s = QuotaSnapshot {
            success: false,
            error: Some("auth failed".into()),
            utilization: None,
            resets_at: None,
            remaining_tokens: None,
            max_tokens: None,
        };
        assert!(!s.is_exhausted());
    }

    #[test]
    fn parse_f64_handles_string_and_number() {
        assert_eq!(parse_f64(&Value::from(100)), Some(100.0));
        assert_eq!(parse_f64(&Value::from("100")), Some(100.0));
        assert_eq!(parse_f64(&Value::Null), None);
    }

    #[test]
    fn extract_reset_time_handles_iso_string() {
        let v = Value::from("2026-07-08T12:00:00Z");
        assert!(extract_reset_time(&v).is_some());
    }

    #[test]
    fn extract_reset_time_handles_millis() {
        let v = Value::from(2_000_000_000_000_i64);
        assert!(extract_reset_time(&v).is_some());
    }

    #[test]
    fn extract_reset_time_handles_seconds() {
        let v = Value::from(1_700_000_000_i64);
        assert!(extract_reset_time(&v).is_some());
    }

    #[test]
    fn extract_reset_time_none_for_zero() {
        let v = Value::from(0);
        assert!(extract_reset_time(&v).is_none());
    }

    #[test]
    fn zhipu_quota_base_routes_by_host() {
        assert_eq!(
            ZhipuQuotaProvider::quota_base("https://open.bigmodel.cn/api/paas/v4"),
            "https://open.bigmodel.cn"
        );
        assert_eq!(
            ZhipuQuotaProvider::quota_base("https://api.z.ai/api/paas/v4"),
            "https://api.z.ai"
        );
    }

    #[test]
    fn minimax_domain_routes_by_host() {
        assert_eq!(
            MinimaxQuotaProvider::domain("https://api.minimaxi.com/v1"),
            "api.minimaxi.com"
        );
        assert_eq!(
            MinimaxQuotaProvider::domain("https://api.minimax.io/v1"),
            "api.minimax.io"
        );
    }
}
