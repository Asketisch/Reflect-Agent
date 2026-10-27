//! `web_fetch` — 抓取 URL 的 HTML 并转 Markdown 返回。
//!
//! v1.0.0-rc4 新增 builtin。流程:`reqwest` GET → 累积 body 至 5 MiB 上限 →
//! `htmd` HTML→Markdown → 截断至 50K 字符。Permission 走 `Prompt`,queue
//! 自动触发 `ApprovalGate.ask_tool`,用户拒绝则 tool 不执行。
//!
//! 安全要点:
//! - 仅允许 `http` / `https` scheme,其余返回 `ToolError::InvalidArgs`。
//! - 每一跳(host 字面量 + DNS 解析结果)都过 [`is_blocked_host`] /
//!   [`is_blocked_ip`] 黑名单:`127.0.0.0/8` / `10.0.0.0/8` /
//!   `172.16.0.0/12` / `192.168.0.0/16` / `169.254.0.0/16` /
//!   `100.64.0.0/10`(CGNAT) / `224.0.0.0/4`(multicast) / `240.0.0.0/4`
//!   (reserved) / `::1` / `fc00::/7` / `fe80::/10` / `ff00::/8` 以及
//!   `localhost` 字面量,防止 agent 借公开 URL → 302 → 内网服务扫到
//!   内部数据。
//! - v1.7 DNS rebinding 防护闭环:hostname 先经 `tokio::net::lookup_host`
//!   解析,**全部**解析结果过黑名单(任一命中即拒),再用
//!   `resolve_to_addrs` 把域名钉定到已校验地址发起连接 —— 关闭「校验时
//!   公网、连接时被 rebinding 到私网」的 TOCTOU 窗口;解析失败按
//!   fail-closed 拒绝。HTTPS 钉定不影响 SNI / 证书校验(仍用域名)。
//! - 重定向手动循环处理:每一跳重新走上面的字面量 + 解析校验,
//!   总跳数上限 [`MAX_REDIRECTS`],防 redirect loop / SSRF 多次试错。
//!
//! 字符截断、body 大小限制均返回 `metadata.truncated = true`,便于上层
//! (microcompact)按需进一步降级。

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use serde_json::Value;
use url::{Host, Url};

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
    /// 基础客户端(IP 字面量 host 直接复用;hostname 走每次请求新建的
    /// 钉定客户端,见 [`Self::pinned_client`])。
    client: reqwest::Client,
}

/// web_fetch 使用的 User-Agent。
///
/// 用主流浏览器 UA 而非 "ZhuYe/1.0":许多站点(arXiv 搜索、USGS
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
        // 重定向手动循环处理(见模块 doc):Policy::none 下 3xx 原样返回,
        // 由 `execute` 逐跳校验后再跟。
        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(DEFAULT_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .expect("reqwest client build");
        Self { client }
    }

    /// 为 hostname 请求构造钉定客户端:域名解析固定为已过黑名单的地址
    /// 列表,连接不可能落到未校验 IP。`addrs` 为 `None`(IP 字面量)时
    /// 不需要钉定。
    fn pinned_client(&self, domain: &str, addrs: &[SocketAddr]) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(DEFAULT_TIMEOUT)
            .redirect(Policy::none());
        if !addrs.is_empty() {
            builder = builder.resolve_to_addrs(domain, addrs);
        }
        builder.build().expect("reqwest pinned client build")
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// DNS 解析 + 全量黑名单校验的中间结果。
#[derive(Debug)]
enum ResolvedHost {
    /// host 本身是 IP 字面量(已过黑名单)。
    Ip,
    /// host 是域名:解析出的全部地址(均已过黑名单),用于钉定。
    Domain(Vec<SocketAddr>),
}

/// 解析并校验 URL host:字面量黑名单 → 域名 DNS 解析 → 全部解析结果
/// 黑名单。任一环节命中内网/保留段即 `Err`(fail-closed;解析失败同样
/// 拒绝 —— 无法证明目标不在内网时不应发起连接)。
async fn resolve_and_validate_host(url: &Url) -> Result<ResolvedHost, ToolError> {
    match url.host() {
        Some(Host::Ipv4(ip)) => {
            validate_ip(IpAddr::V4(ip))?;
            Ok(ResolvedHost::Ip)
        }
        Some(Host::Ipv6(ip)) => {
            validate_ip(IpAddr::V6(ip))?;
            Ok(ResolvedHost::Ip)
        }
        Some(Host::Domain(domain)) => {
            if is_blocked_host(domain) {
                return Err(ToolError::InvalidArgs {
                    message: format!("host '{domain}' is in a blocked range"),
                });
            }
            // 端口不参与解析语义,占 0 即可。
            let addrs = tokio::net::lookup_host((domain, 0u16))
                .await
                .map_err(|e| ToolError::Io(format!("DNS 解析失败 '{domain}': {e}")))?
                .collect::<Vec<_>>();
            validate_resolved_addrs(domain, &addrs)
                .map_err(|m| ToolError::InvalidArgs { message: m })?;
            Ok(ResolvedHost::Domain(addrs))
        }
        None => Err(ToolError::InvalidArgs {
            message: "URL 缺少 host".into(),
        }),
    }
}

/// 纯函数:校验全部解析地址,任一命中黑名单即拒。单独抽出便于对
/// 合成地址做白盒测试(不依赖真实 DNS)。
fn validate_resolved_addrs(domain: &str, addrs: &[SocketAddr]) -> Result<(), String> {
    if addrs.is_empty() {
        return Err(format!("DNS 解析 '{domain}' 结果为空,拒绝连接"));
    }
    for addr in addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(format!(
                "host '{domain}' 解析到被封锁地址 {}(DNS rebinding 防护)",
                addr.ip()
            ));
        }
    }
    Ok(())
}

/// IP 黑名单的 `ToolError` 薄封装。
fn validate_ip(ip: IpAddr) -> Result<(), ToolError> {
    if is_blocked_ip(&ip) {
        Err(ToolError::InvalidArgs {
            message: format!("host '{ip}' is in a blocked range"),
        })
    } else {
        Ok(())
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.web_fetch")
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

    fn required_permission(&self) -> zhuye_protocol::PermissionMode {
        zhuye_protocol::PermissionMode::Prompt
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

        // v1.7:手动重定向循环 —— 每一跳都重新做「字面量黑名单 + DNS 解析
        // 全量校验 + 地址钉定」,重定向目标同样受 DNS rebinding 防护约束。
        let mut current = url.clone();
        let mut resp = None;
        for _hop in 0..=MAX_REDIRECTS {
            let resolved = resolve_and_validate_host(&current).await?;
            let client = match &resolved {
                ResolvedHost::Ip => self.client.clone(),
                ResolvedHost::Domain(addrs) => {
                    let domain = match current.host() {
                        Some(Host::Domain(d)) => d.to_string(),
                        _ => unreachable!("ResolvedHost::Domain 必然对应 Domain host"),
                    };
                    self.pinned_client(&domain, addrs)
                }
            };
            let send = tokio::time::timeout(timeout, client.get(current.clone()).send())
                .await
                .map_err(|_| ToolError::Timeout {
                    elapsed_ms: timeout.as_millis() as u64,
                })?
                .map_err(|e| ToolError::Io(format!("request failed: {e}")))?;

            if send.status().is_redirection() {
                let loc = send
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| ToolError::Io("redirect without Location header".into()))?
                    .to_str()
                    .map_err(|_| ToolError::Io("Location header 非 ASCII".into()))?
                    .to_string();
                let next = current
                    .join(&loc)
                    .map_err(|e| ToolError::Io(format!("redirect Location 解析失败: {e}")))?;
                if !ALLOWED_SCHEMES.contains(&next.scheme()) {
                    return Err(ToolError::InvalidArgs {
                        message: format!(
                            "redirect scheme '{}' not allowed (use http/https)",
                            next.scheme()
                        ),
                    });
                }
                current = next;
                continue;
            }
            resp = Some(send);
            break;
        }
        let mut resp =
            resp.ok_or_else(|| ToolError::Io(format!("too many redirects (> {MAX_REDIRECTS})")))?;

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
            // 0.8:`&markdown[..N]` 在 N 不是 char 边界时 panic。中文页面
            // (常见场景)一个汉字 3 字节,固定 50_000 的字节偏移极易切在
            // 汉字中间。改用协议层的安全截断。
            let mut s = zhuye_protocol::truncate_str(&markdown, MAX_OUTPUT_CHARS).to_string();
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
            content: vec![zhuye_protocol::ContentBlock::text(body_md)],
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> ToolContext {
        ToolContext::for_workspace(".")
    }

    // ── scheme / host 校验 ────────────────────────────────────────────

    /// GAIA-fix 回归:web_fetch 必须用浏览器 UA,而非 "ZhuYe/1.0"。
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
            !BROWSER_UA.contains("ZhuYe"),
            "UA 不应含 bot 标识 'ZhuYe/1.0',实际: {BROWSER_UA}"
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

    // ── v1.7 DNS rebinding 防护(解析后全量校验) ─────────────────────

    /// 纯函数:全部解析地址过黑名单 —— 任一私网/CGNAT/多播地址即拒。
    #[test]
    fn validate_resolved_addrs_blocks_private_and_mixed() {
        use std::net::SocketAddr;
        let sa = |ip: &str| SocketAddr::new(ip.parse().unwrap(), 443);
        // 纯公网 → 放行。
        assert!(validate_resolved_addrs("ok.com", &[sa("93.184.216.34")]).is_ok());
        assert!(
            validate_resolved_addrs("ok.com", &[sa("2606:2800:220:1::1")]).is_ok(),
            "公网 IPv6 应放行"
        );
        // 私网 → 拒。
        for bad in [
            "10.0.0.5",
            "172.16.0.9",
            "192.168.1.10",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "fd00::1",
        ] {
            assert!(
                validate_resolved_addrs("evil.com", &[sa(bad)]).is_err(),
                "{bad} 应被拒"
            );
        }
        // 混合记录(先公网后私网)→ 拒(不能只看第一条)。
        let mixed = [sa("93.184.216.34"), sa("10.0.0.5")];
        assert!(
            validate_resolved_addrs("rebind.com", &mixed).is_err(),
            "混合记录应整批拒绝"
        );
        // 空解析结果 → fail-closed 拒绝。
        assert!(validate_resolved_addrs("empty.com", &[]).is_err());
    }

    /// `localhost` 走 Domain 分支:字符串黑名单先拒,不触发真实解析。
    /// (防御深度:即使黑名单漏掉,解析校验仍会拦 127.0.0.1。)
    #[tokio::test]
    async fn domain_branch_rejects_localhost_before_dns() {
        let url = Url::parse("http://localhost:8080/").unwrap();
        assert!(matches!(url.host(), Some(Host::Domain(_))));
        let err = resolve_and_validate_host(&url)
            .await
            .expect_err("localhost 应被拒");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
    }

    /// IP 字面量走 Ip 分支校验(loopback 拒 / 公网放行),不触发 DNS。
    #[tokio::test]
    async fn ip_branch_validates_literal() {
        let bad = Url::parse("http://127.0.0.1:9/").unwrap();
        assert!(resolve_and_validate_host(&bad).await.is_err());
        // 公网字面量只做校验,不发起连接(本函数只解析,不请求)。
        let ok = Url::parse("http://93.184.216.34/").unwrap();
        assert!(resolve_and_validate_host(&ok).await.is_ok());
    }

    // ── 纯函数白盒测试(is_blocked_host / sanitize_url_for_metadata) ──
    // 注:这些函数已下沉到 `crate::builtins::url_safety`,其自有测试模块
    // 覆盖;本模块只保留走 `execute()` 的集成路径测试。

    // ── 真实网络测试(`#[ignore]`,需 --include-ignored 跑)────────────

    #[tokio::test]
    #[ignore = "real-network test, run with: cargo test -p zhuye-tools -- --ignored fetches_html_and_returns_markdown"]
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
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
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
    /// 回归(0.8):非 ASCII 页面的 markdown 截断不得 panic。
    /// 修复前 `&markdown[..MAX_OUTPUT_CHARS]` 在偏移不是 char 边界时
    /// 直接 panic —— 中文页面(高频场景)一个汉字 3 字节,固定的
    /// 50_000 字节偏移极易切在汉字中间。
    #[test]
    fn truncation_survives_multibyte_markdown() {
        // 直接测内部截断逻辑:复用 execute 里那段判断的等价形式。
        // 构造远超 MAX_OUTPUT_CHARS 的中文串。
        let markdown = "中".repeat(super::MAX_OUTPUT_CHARS * 2);
        let (body_md, truncated_md) = if markdown.len() > super::MAX_OUTPUT_CHARS {
            let mut s =
                zhuye_protocol::truncate_str(&markdown, super::MAX_OUTPUT_CHARS).to_string();
            s.push_str(&format!(
                "\n\n[... truncated to {} chars ...]",
                super::MAX_OUTPUT_CHARS
            ));
            (s, true)
        } else {
            (markdown.clone(), false)
        };
        assert!(truncated_md);
        assert!(body_md.contains('中'), "中文内容应完整保留");
        assert!(
            body_md.len() > super::MAX_OUTPUT_CHARS,
            "截断部分 + 标记应超过上限"
        );
    }

    /// 回归(0.8):emoji(4 字节)同样不得 panic。
    #[test]
    fn truncation_survives_4byte_markdown() {
        let markdown = "\u{1F600}".repeat(super::MAX_OUTPUT_CHARS * 2);
        let out = zhuye_protocol::truncate_str(&markdown, super::MAX_OUTPUT_CHARS);
        assert!(out.len() <= super::MAX_OUTPUT_CHARS);
        assert!(out.contains('\u{1F600}'));
    }
}
