//! `web_fetch` — 抓取 URL 的 HTML 并转 Markdown 返回。
//!
//! v1.0.0-rc4 新增 builtin。流程:`reqwest` GET → 累积 body 至 5 MiB 上限 →
//! `htmd` HTML→Markdown → 截断至 50K 字符。Permission 走 `Prompt`,queue
//! 自动触发 `ApprovalGate.ask_tool`,用户拒绝则 tool 不执行。
//!
//! 安全要点:
//! - 仅允许 `http` / `https` scheme,其余返回 `ToolError::InvalidArgs`。
//! - 自定义 redirect policy:每跳重新校验 host,拦截 `127.0.0.0/8` /
//!   `10.0.0.0/8` / `172.16.0.0/12` / `192.168.0.0/16` / `169.254.0.0/16` /
//!   `100.64.0.0/10`(CGNAT) / `224.0.0.0/4`(multicast) / `240.0.0.0/4`(reserved)
//!   / `::1` / `fc00::/7` / `fe80::/10` / `ff00::/8` 以及 `localhost` 字面量,
//!   防止 agent 借公开 URL → 302 → 内网服务扫到内部数据。
//! - 总跳转数上限 [`MAX_REDIRECTS`],防止 redirect loop / SSRF 多次试错。
//!
//! 已知局限(DNS rebinding):`is_blocked_host` 仅校验 host 字符串字面量。
//! 攻击者控制 `attacker.com` 解析到 `10.0.0.5` 的攻击面需在 v1.1 引入
//! `hickory-resolver` 做解析后 IP 比对才能闭环。当前版本依赖 `Prompt` 权限
//! 门 + 用户人工审核兜底。
//!
//! 字符截断、body 大小限制均返回 `metadata.truncated = true`,便于上层
//! (microcompact)按需进一步降级。

use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use serde_json::Value;
use url::Url;

use crate::builtins::url_safety::*;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// 单次响应最大读取字节数(5 MiB)—— 防止恶意/巨型页面吃光内存。
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Markdown 输出字符上限。超出后截断并在末尾追加标记。
const MAX_OUTPUT_CHARS: usize = 50_000;

/// 默认超时上限。即便 `ctx.timeout` 给了 60 秒,WebFetch 也至少 30 秒封顶。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 单次请求允许的最大重定向次数。超过则请求中断(返回 Error)。
const MAX_REDIRECTS: usize = 5;

const ALLOWED_SCHEMES: &[&str] = &["http", "https"];

pub struct WebFetchTool {
    client: reqwest::Client,
}

/// web_fetch 使用的 User-Agent。
///
/// 用主流浏览器 UA 而非 "Reflect-Agent/1.0":许多站点(arXiv 搜索、USGS
/// NAS、Google、Wikipedia 等)对非浏览器 UA 返回 JS 脚手架/精简页/直接
/// 拦截,导致 web_fetch 拿不到服务端渲染的内容。浏览器 UA 让站点返回
/// 完整的服务端渲染 HTML,这是网络抓取的标准做法。
///
/// GAIA 验证(commit 历史):bot UA 抓 arXiv 搜索页只返回 MathJax 脚手架,
/// 无结果列表;浏览器 UA 能拿到服务端渲染的论文 ID 列表。USGS NAS 物种
/// 档案页同理 —— bot UA 返回 `window.dataLayer` 脚手架,浏览器 UA 返回
/// 真实物种数据。
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(DEFAULT_TIMEOUT)
            .redirect(Policy::custom(|attempt| {
                // `previous()` 已包含初始 URL,所以 0..=MAX_REDIRECTS 累计
                // 至多 MAX_REDIRECTS + 1 个请求,与 Policy::limited 语义对齐。
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                let is_blocked = attempt
                    .url()
                    .host_str()
                    .map(is_blocked_host)
                    .unwrap_or(false);
                if is_blocked {
                    let host = attempt.url().host_str().unwrap_or("").to_string();
                    return attempt
                        .error(format!("redirect target '{host}' is in a blocked range"));
                }
                attempt.follow()
            }))
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.web_fetch")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url":     {"type": "string", "description": "Absolute http/https URL to fetch"},
                "timeout": {"type": "integer", "description": "Override timeout in seconds (≤ 120)"},
            },
            "required": ["url"],
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 出站网络副作用 → 与其他并发调用隔离。
        false
    }

    fn required_permission(&self) -> reflect_protocol::PermissionMode {
        reflect_protocol::PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let raw_url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'url'".into(),
            })?
            .to_string();

        let span = tracing::info_span!(
            "web_fetch.execute",
            url = %raw_url,
            host = tracing::field::Empty,
            status = tracing::field::Empty,
        );
        let _enter = span.enter();

        let url = Url::parse(&raw_url).map_err(|e| ToolError::InvalidArgs {
            message: format!("invalid url: {e}"),
        })?;

        let scheme = url.scheme();
        if !ALLOWED_SCHEMES.contains(&scheme) {
            return Err(ToolError::InvalidArgs {
                message: format!("scheme '{scheme}' not allowed (use http/https)"),
            });
        }

        if let Some(host) = url.host_str() {
            reject_private_hosts(host)?;
        }

        // 用户可在 args 里覆盖 timeout,但不能超过 120 秒,也不能超过 ctx.timeout。
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TIMEOUT)
            .min(Duration::from_secs(120))
            .min(if ctx.timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                ctx.timeout
            });

        let resp = tokio::time::timeout(timeout, self.client.get(url.clone()).send())
            .await
            .map_err(|_| ToolError::Timeout {
                elapsed_ms: timeout.as_millis() as u64,
            })?
            .map_err(|e| ToolError::Io(format!("request failed: {e}")))?;

        // 记录最终 host / status 到 span 字段,便于事后安全取证。
        let final_url = resp.url().clone();
        if let Some(host) = final_url.host_str() {
            tracing::Span::current().record("host", host);
        }
        tracing::Span::current().record("status", resp.status().as_u16());

        let status = resp.status();
        if !status.is_success() {
            // 不回显完整 URL 到 ToolError 消息(避免 query string 里的 secret
            // 落进 LLM transcript / PostToolUse hook 日志)。
            return Err(ToolError::Io(format!(
                "HTTP {status} from {host}",
                host = final_url.host_str().unwrap_or("remote"),
            )));
        }

        // 累积 body 直到 MAX_BODY_BYTES 上限(reqwest 0.12 不直接提供
        // bytes_with_limit,故手动用 chunk() 累加)。
        let mut bytes = Vec::new();
        let mut truncated_body = false;
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ToolError::Io(format!("body read: {e}")))?
        {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                let remaining = MAX_BODY_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated_body = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }

        let html = String::from_utf8_lossy(&bytes);
        let markdown = htmd::convert(&html).unwrap_or_else(|_| html.into_owned());

        let (body_md, truncated_md) = if markdown.len() > MAX_OUTPUT_CHARS {
            let mut s = markdown[..MAX_OUTPUT_CHARS].to_string();
            s.push_str(&format!(
                "\n\n[... truncated to {MAX_OUTPUT_CHARS} chars ...]"
            ));
            (s, true)
        } else {
            (markdown, false)
        };

        let truncated = truncated_body || truncated_md;

        // metadata.url 必须脱敏:query string / fragment 可能携带 secret(如
        // `?token=...`、`#access_token=...`),直接写入 metadata 会经
        // PostToolUse hooks / TUI transcript / LLM context 漏出。
        let safe_url = sanitize_url_for_metadata(&final_url);

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(body_md)],
            is_error: false,
            metadata: serde_json::json!({
                "url": safe_url,
                "status": status.as_u16(),
                "bytes_read": bytes.len(),
                "truncated": truncated,
                "body_truncated": truncated_body,
                "markdown_truncated": truncated_md,
            }),
            elapsed_ms: 0,
        })
    }
}

/// 把 `is_blocked_host` 包成 `ToolError::InvalidArgs` 的薄封装 —— `url_safety`
/// 子模块保持纯函数依赖,不反向依赖 `tool::ToolError`,所以本层 wrapper
/// 留在 web_fetch.rs。
fn reject_private_hosts(host: &str) -> Result<(), ToolError> {
    if is_blocked_host(host) {
        Err(ToolError::InvalidArgs {
            message: format!("host '{host}' is in a blocked range"),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext {
        ToolContext::for_workspace(".")
    }

    // ── scheme / host 校验 ────────────────────────────────────────────

    /// GAIA-fix 回归:web_fetch 必须用浏览器 UA,而非 "Reflect-Agent/1.0"。
    /// bot UA 会被 arXiv/USGS/Google 等站点返回 JS 脚手架/精简页,导致
    /// 拿不到服务端渲染内容。断言常量含浏览器标识(Mozilla + AppleWebKit)。
    #[test]
    fn user_agent_is_browser_not_bot() {
        assert!(
            BROWSER_UA.starts_with("Mozilla/5.0"),
            "UA 应以 Mozilla/5.0 开头(浏览器标识),实际: {BROWSER_UA}"
        );
        assert!(
            BROWSER_UA.contains("AppleWebKit"),
            "UA 应含 AppleWebKit(浏览器引擎标识),实际: {BROWSER_UA}"
        );
        assert!(
            !BROWSER_UA.contains("Reflect-Agent"),
            "UA 不应含 bot 标识 'Reflect-Agent',实际: {BROWSER_UA}"
        );
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({"url": "ftp://example.com/"}))
            .await
            .expect_err("ftp should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// `Url::parse` 自动归一化 scheme 为小写,故 `HTTP://` 也命中白名单检查。
    /// 本测试仅断言 url crate 的归一化行为 —— 不调用 `execute`,避免触发
    /// 真实网络请求。
    #[tokio::test]
    async fn uppercase_scheme_normalizes_and_runs_host_check() {
        let parsed = url::Url::parse("HTTP://example.com/").expect("parse ok");
        assert_eq!(parsed.scheme(), "http");
    }

    #[tokio::test]
    async fn rejects_loopback_ip_literal() {
        let t = WebFetchTool::new();
        let err = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "http://127.0.0.1:8080/secret"}),
            )
            .await
            .expect_err("loopback should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn rejects_localhost_string() {
        let t = WebFetchTool::new();
        let err = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "http://localhost:8080/"}),
            )
            .await
            .expect_err("localhost should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn rejects_private_rfc1918() {
        let t = WebFetchTool::new();
        for host in ["10.0.0.1", "172.16.0.1", "192.168.1.1", "169.254.1.1"] {
            let err = t
                .execute(
                    make_ctx(),
                    serde_json::json!({"url": format!("http://{host}/")}),
                )
                .await
                .expect_err("{host} should be rejected");
            assert!(
                matches!(err, ToolError::InvalidArgs { .. }),
                "{host}: got {err:?}"
            );
        }
    }

    /// IPv6 loopback `[::1]` 应被拦下 —— 现有 6 条单测仅覆盖 IPv4。
    #[tokio::test]
    async fn rejects_ipv6_loopback_literal() {
        let t = WebFetchTool::new();
        let err = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "http://[::1]:8080/secret"}),
            )
            .await
            .expect_err("IPv6 loopback should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// IPv6 ULA `fc00::/7`(`fd00::/8` 是常见子集)应被拦下。
    #[tokio::test]
    async fn rejects_ipv6_ula_literal() {
        let t = WebFetchTool::new();
        let err = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "http://[fd00::1]/secret"}),
            )
            .await
            .expect_err("IPv6 ULA should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// IPv6 link-local 带 zone-id(`%eth0`)应被拦下 —— 验证 zone-id
    /// 剥离逻辑。
    #[tokio::test]
    async fn rejects_ipv6_link_local_with_zone_id() {
        let t = WebFetchTool::new();
        let err = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "http://[fe80::1%eth0]/"}),
            )
            .await
            .expect_err("IPv6 link-local with zone-id should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// CGNAT `100.64.0.0/10`(`100.64.0.1`)应被拦下 —— 此前未拦。
    #[tokio::test]
    async fn rejects_cgnat_100_64_0_1() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({"url": "http://100.64.0.1/"}))
            .await
            .expect_err("CGNAT should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// IPv4 multicast `224.0.0.0/4`(`224.0.0.1`)应被拦下 —— 此前未拦。
    #[tokio::test]
    async fn rejects_ipv4_multicast_224_0_0_1() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({"url": "http://224.0.0.1/"}))
            .await
            .expect_err("IPv4 multicast should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// IPv6 multicast `ff00::/8`(`ff02::1`)应被拦下 —— 此前未拦。
    #[tokio::test]
    async fn rejects_ipv6_multicast_ff02_1() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({"url": "http://[ff02::1]/"}))
            .await
            .expect_err("IPv6 multicast should be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn missing_url_arg_is_invalid_args() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({}))
            .await
            .expect_err("missing url should error");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn invalid_url_is_invalid_args() {
        let t = WebFetchTool::new();
        let err = t
            .execute(make_ctx(), serde_json::json!({"url": "not a url"}))
            .await
            .expect_err("invalid url should error");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    // ── 纯函数白盒测试(is_blocked_host / sanitize_url_for_metadata) ──
    // 注:这些函数已下沉到 `crate::builtins::url_safety`,其自有测试模块
    // 覆盖;本模块只保留走 `execute()` 的集成路径测试。

    // ── 真实网络测试(`#[ignore]`,需 --include-ignored 跑)────────────

    #[tokio::test]
    #[ignore = "real-network test, run with: cargo test -p reflect-tools -- --ignored fetches_html_and_returns_markdown"]
    async fn fetches_html_and_returns_markdown() {
        // 真实抓取 example.com —— 公共站点且响应稳定。默认忽略避免 CI 受
        // 网络影响;手动验证 web_fetch 端到端时 `--ignored` 跑这一条即可。
        let t = WebFetchTool::new();
        let out = t
            .execute(
                make_ctx(),
                serde_json::json!({"url": "https://example.com/"}),
            )
            .await
            .expect("example.com should be reachable");
        let body = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(
            body.to_lowercase().contains("example"),
            "body should contain 'example', got: {body}"
        );
        assert_eq!(
            out.metadata.get("status").and_then(|v| v.as_u64()),
            Some(200)
        );
        // metadata.url 经 sanitize,不应带 query string。
        let safe_url = out.metadata.get("url").and_then(|v| v.as_str()).unwrap();
        assert_eq!(safe_url, "https://example.com/");
    }
}
