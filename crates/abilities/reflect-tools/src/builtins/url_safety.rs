//! 出站 URL 共用安全工具 —— 给 `web_fetch` / `web_search` 等 builtin
//! 共享的 SSRF 黑名单 + URL 消毒纯函数。
//!
//! 设计要点:
//! - 所有函数都是纯函数,无 `ToolError` 依赖,可以无副作用地被任何工具复用。
//! - 把 `is_blocked_host` / `is_blocked_ip` / `is_blocked_v4` /
//!   `is_blocked_v6` / `is_cgnat_v4` / `is_multicast_v4` /
//!   `is_reserved_v4` 与 `sanitize_url_for_metadata` 集中到本模块,
//!   避免在每个 builtin 里重复实现,产生多份不同步的 URL 校验/消毒逻辑。
//! - 顶层 `web_fetch.rs` 保留一个 `reject_private_hosts(host) ->
//!   Result<(), ToolError>` 薄封装,把纯函数包成 `ToolError::InvalidArgs`,
//!   这样本模块不会反向依赖 `tool::ToolError`,保持可移植。
//!
//! 黑名单覆盖范围(参考 web_fetch 模块 doc):
//! - IPv4:loopback / RFC1918 / link-local / unspecified / broadcast /
//!   documentation / CGNAT(RFC 6598) / multicast / reserved。
//! - IPv6:loopback / unspecified / ULA(fc00::/7)/ link-local(fe80::/10)/
//!   multicast(ff00::/8)。
//! - 字面量:`localhost` / `ip6-localhost` / `ip6-loopback`。
//!
//! 已知局限(DNS rebinding):`is_blocked_host` 仅校验 host 字符串字面量,
//! 真实 hostname → 私网 IP 解析后比对留 v1.1 引入 `hickory-resolver`。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

/// SSRF 防护 v1:对 host 字符串做白名单/黑名单校验。
///
/// 返回 `true` 当 host 命中任意内网/loopback/multicast/reserved 范围。
///
/// `url::Url::host_str()` 对 IPv6 字面量返回**带方括号**的形式(如
/// `"[::1]"`、`"[fe80::1%eth0]"`),需要先剥方括号;IPv6 zone-id
/// (`fe80::1%eth0`)还要再剥 `%eth0` 后缀才能 parse 成 `Ipv6Addr`。
///
/// DNS rebinding 防护(hostname → 私网 IP 解析后比对)留作 v1.1。
pub fn is_blocked_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) {
        return true;
    }
    // IPv6 字面量:剥方括号 → `::1` / `fe80::1%eth0`
    let without_brackets = lower
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&lower);
    // IPv6 zone-id 剥离:`fe80::1%eth0` → `fe80::1`
    let host_no_zone = without_brackets
        .split('%')
        .next()
        .unwrap_or(without_brackets);
    if let Ok(ip) = host_no_zone.parse::<IpAddr>() {
        return is_blocked_ip(&ip);
    }
    // host 是 hostname(不是 IP 字面量):留给 v1.1 DNS 解析后比对。
    false
}

pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

pub fn is_blocked_v4(v4: &Ipv4Addr) -> bool {
    v4.is_loopback()                // 127.0.0.0/8
        || v4.is_private()          // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()       // 169.254/16(涵盖 AWS/GCP/Azure metadata)
        || v4.is_unspecified()      // 0.0.0.0
        || v4.is_broadcast()        // 255.255.255.255
        || v4.is_documentation()    // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || is_cgnat_v4(v4)          // 100.64.0.0/10(RFC 6598,共享地址空间)
        || is_multicast_v4(v4)      // 224.0.0.0/4
        || is_reserved_v4(v4) // 240.0.0.0/4
}

pub fn is_blocked_v6(v6: &Ipv6Addr) -> bool {
    v6.is_loopback()
        || v6.is_unspecified()
        || (v6.segments()[0] & 0xfe00) == 0xfc00  // fc00::/7 ULA
        || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (v6.segments()[0] & 0xff00) == 0xff00 // ff00::/8 multicast
}

/// CGNAT / 共享地址空间(RFC 6598):100.64.0.0/10。
fn is_cgnat_v4(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] >= 64 && o[1] <= 127)
}

/// IPv4 多播:224.0.0.0/4。
fn is_multicast_v4(v4: &Ipv4Addr) -> bool {
    v4.octets()[0] >= 224 && v4.octets()[0] <= 239
}

/// IPv4 保留段:240.0.0.0/4(包括 255.255.255.255 broadcast,但 std
/// 已经把 255.255.255.255 单独归到 `is_broadcast()`)。
fn is_reserved_v4(v4: &Ipv4Addr) -> bool {
    v4.octets()[0] >= 240
}

/// 剥除 URL 的 query string / fragment / userinfo,返回不含 secret 的安全形式。
///
/// 用于 `ToolOutput.metadata.url` 与 `web_search` 结果文本 —— 防止
/// PostToolUse hook 日志 / TUI transcript / LLM context 看到
/// `?token=eyJ...` / `#access_token=...` 这类敏感字段。用户级 password
/// (`user:pass@host`)也一并剥除:`Url::set_password(None)` 仅影响后续
/// to_string,不会反向暴露已存在的 password。
pub fn sanitize_url_for_metadata(url: &Url) -> String {
    let mut sanitized = url.clone();
    sanitized.set_query(None);
    sanitized.set_fragment(None);
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);
    sanitized.as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 字面量 / 大小写 ──────────────────────────────────────────────

    #[test]
    fn blocks_localhost_strings() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("LOCALHOST"));
        assert!(is_blocked_host("ip6-localhost"));
        assert!(is_blocked_host("ip6-loopback"));
    }

    // ── IPv4 范围 ──────────────────────────────────────────────────

    #[test]
    fn blocks_ipv4_ranges() {
        for ip in &[
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.169.254", // AWS / GCP / Azure metadata
            "0.0.0.0",
            "255.255.255.255",
            "192.0.2.1",       // TEST-NET-1
            "198.51.100.1",    // TEST-NET-2
            "203.0.113.1",     // TEST-NET-3
            "100.64.0.1",      // CGNAT
            "100.127.255.255", // CGNAT 上界
            "224.0.0.1",       // IPv4 multicast
            "239.255.255.255", // IPv4 multicast 上界
            "240.0.0.1",       // IPv4 reserved
            "255.255.255.254", // IPv4 reserved
        ] {
            assert!(is_blocked_host(ip), "expected blocked: {ip}");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for ip in &[
            "8.8.8.8",
            "1.1.1.1",
            "9.9.9.9",
            "100.63.255.255",
            "100.128.0.0",
        ] {
            assert!(!is_blocked_host(ip), "expected allowed: {ip}");
        }
    }

    // ── IPv6 范围 ──────────────────────────────────────────────────

    #[test]
    fn blocks_ipv6_ranges() {
        for ip in &[
            "::1",
            "::",
            "[::1]",
            "[fe80::1]",
            "[fe80::1%eth0]", // 带 zone-id
            "[fc00::1]",
            "[fd00::1]",
            "[ff02::1]",
            "[ff00::1]",
        ] {
            assert!(is_blocked_host(ip), "expected blocked: {ip}");
        }
    }

    #[test]
    fn allows_public_ipv6() {
        for ip in &["[2606:4700:4700::1111]", "[2001:4860:4860::8888]"] {
            assert!(!is_blocked_host(ip), "expected allowed: {ip}");
        }
    }

    // ── 主机名判定 ───────────────────────────────────────────────────

    #[test]
    fn allows_hostname_names() {
        // hostname 字面量无法做 IP 比对,留给 v1.1 DNS 解析。
        assert!(!is_blocked_host("example.com"));
        assert!(!is_blocked_host("github.com"));
    }

    // ── metadata 用的 URL 脱敏 ─────────────────────────────────────

    #[test]
    fn sanitize_url_strips_query_and_fragment() {
        let url =
            Url::parse("https://api.example.com/data?token=eyJabc.sig&user=42#section").unwrap();
        assert_eq!(
            sanitize_url_for_metadata(&url),
            "https://api.example.com/data"
        );
    }

    #[test]
    fn sanitize_url_strips_userinfo() {
        let url = Url::parse("https://user:pass@api.example.com/").unwrap();
        assert_eq!(sanitize_url_for_metadata(&url), "https://api.example.com/");
    }

    #[test]
    fn sanitize_url_preserves_path_and_port() {
        let url = Url::parse("https://api.example.com:8443/v2/items/42").unwrap();
        assert_eq!(
            sanitize_url_for_metadata(&url),
            "https://api.example.com:8443/v2/items/42"
        );
    }
}
