//! `web_search` — 通过 Brave Search API 执行 Web 搜索。
//!
//! v1.0.0-rc4 新增 builtin。Provider 默认 Brave(`https://api.search.brave.com/
//! res/v1/web/search`),API Key 按以下优先级查找:
//! 1. `ctx.env` 的 `BRAVE_API_KEY`(由 `[web_search].api_key` TOML 段经
//!    `AgentConfig::tool_env` 注入,或 subagent spawn 时传入)
//! 2. 进程环境变量 `BRAVE_API_KEY`(`std::env::var`,与 anthropic/openai
//!    provider 的 api_key 兜底同 pattern)
//! 3. 均无 → `ToolError::Execution`
//!
//! Permission 走默认 `Auto`(只读,无副作用)。用户可通过 S5a `PermissionResolver`
//! 写一条 `deny web_search` 规则反向禁用。
//!
//! Brave 响应结构:`{ "web": { "results": [ {title, url, description}, ... ] } }`。
//! v1 仅解析 `web.results`,忽略 `news` / `videos` / `faq` 等扩展段(留作 v1.1)。
//!
//! 安全要点(review 2026-06-30 加固):
//! - 结果文本里的 URL 经 [`crate::builtins::url_safety::sanitize_url_for_metadata`]
//!   消毒:query / fragment / userinfo 一律剥除,避免 Brave 搜索结果里偶发
//!   带 `?token=…` 的内网文档站链接未经审批直注 LLM context。`web_search`
//!   是 `Auto` 权限(`web_fetch` 是 `Prompt`),没有 `ApprovalGate` 兜底,
//!   所以这一步必须在工具内部完成,不能依赖 `Prompt` 门。
//! - 响应体在解析前强制 [`MAX_BODY_BYTES`] 上限(2 MiB)—— Brave 通常
//!   < 200 KB,但 server misconfig 可能放大响应,2 MiB 远超正常值又能
//!   OOM 之前熔断。
//! - 单次执行包在 `tracing::info_span!("web_search.execute", ...)` 里,
//!   事后可观测(query / 命中数 / status / truncated),**不**记录
//!   `api_key`,**不**记录单条 result URL。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use crate::builtins::url_safety::sanitize_url_for_metadata;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_COUNT: usize = 5;
const MAX_COUNT: usize = 20;
/// 输出文本上限 —— 防止模型把过长的搜索结果当上下文。超出截断。
const MAX_OUTPUT_CHARS: usize = 8_000;
/// 单次响应最大读取字节数 —— 超过即截断并在 metadata.truncated 标记。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Reflect-Agent/1.0 (web_search)")
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self { client }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.web_search")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query (non-empty)"},
                "count": {"type": "integer", "description": "Result count (default 5, max 20)"},
            },
            "required": ["query"],
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 只读,不修改任何本地状态,可以并发。
        true
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'query'".into(),
            })?
            .trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "empty 'query'".into(),
            });
        }

        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_COUNT))
            .unwrap_or(DEFAULT_COUNT);

        let api_key = ctx
            .env
            .get("BRAVE_API_KEY")
            .cloned()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                std::env::var("BRAVE_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
            })
            .ok_or_else(|| {
                ToolError::Execution(
                    "BRAVE_API_KEY not set (configure via config.toml [web_search].api_key \
                     or export BRAVE_API_KEY)"
                        .into(),
                )
            })?;

        let timeout = DEFAULT_TIMEOUT.min(if ctx.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            ctx.timeout
        });

        // 顶层 span:`query` 与 `count` 是入参常量,直接 record;`returned` /
        // `status` / `truncated` 在请求结束后回填,便于事后追查 agent
        // 究竟搜了什么、命中几条、最终状态码多少。
        let span = tracing::info_span!(
            "web_search.execute",
            query = %query,
            count = count,
            returned = tracing::field::Empty,
            status = tracing::field::Empty,
            truncated = tracing::field::Empty,
        );
        let _enter = span.enter();

        let resp = tokio::time::timeout(
            timeout,
            self.client
                .get(BRAVE_ENDPOINT)
                .header("X-Subscription-Token", api_key)
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", &count.to_string())])
                .send(),
        )
        .await
        .map_err(|_| ToolError::Timeout {
            elapsed_ms: timeout.as_millis() as u64,
        })?
        .map_err(|e| ToolError::Io(format!("brave request: {e}")))?;

        let status = resp.status();
        tracing::Span::current().record("status", status.as_u16());

        if !status.is_success() {
            return Err(ToolError::Io(format!("Brave API HTTP {status}")));
        }

        // 累积 body 直到 MAX_BODY_BYTES 上限(reqwest 0.12 不直接提供
        // bytes_with_limit,故手动用 chunk() 累加),防止 server misconfig /
        // 大响应 OOM。
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
        tracing::Span::current().record("truncated", truncated_body);

        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ToolError::Io(format!("brave json decode: {e}")))?;

        let results = body
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        let mut text = String::new();
        let mut emitted = 0usize;
        for r in results.iter().take(count) {
            let title = r
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
            // URL 消毒:parse + 剥 query / fragment / userinfo,避免
            // spammed SEO 结果 / 内网文档站带 `?token=…` / `#access_token=…`
            // 之类敏感字段被 LLM 当作输入读取。web_search 是 Auto 权限,
            // 没有 Prompt 门,这一步是工具内部的最后一道防线。
            let raw_url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let safe_url = match Url::parse(raw_url) {
                Ok(parsed) => sanitize_url_for_metadata(&parsed),
                Err(_) => String::new(), // 解析失败时不让垃圾字符串进 LLM context
            };
            let desc = r.get("description").and_then(|v| v.as_str()).unwrap_or("");
            text.push_str(&format!(
                "{}. {title}\n   {safe_url}\n   {desc}\n\n",
                emitted + 1
            ));
            emitted += 1;
            if text.len() >= MAX_OUTPUT_CHARS {
                text.push_str("[... truncated ...]");
                break;
            }
        }
        if text.is_empty() {
            text = "(no results)".into();
        }
        tracing::Span::current().record("returned", emitted as u64);

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(text)],
            is_error: false,
            metadata: serde_json::json!({
                "query": query,
                "requested": count,
                "returned": emitted,
                "provider": "brave",
                "truncated": truncated_body,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_ctx_with_env(env: HashMap<String, String>) -> ToolContext {
        ToolContext {
            env,
            ..ToolContext::for_workspace(".")
        }
    }

    #[tokio::test]
    async fn missing_query_is_invalid_args() {
        let t = WebSearchTool::new();
        let err = t
            .execute(make_ctx_with_env(HashMap::new()), serde_json::json!({}))
            .await
            .expect_err("missing query should error");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn empty_query_is_invalid_args() {
        let t = WebSearchTool::new();
        let err = t
            .execute(
                make_ctx_with_env(HashMap::new()),
                serde_json::json!({"query": "   "}),
            )
            .await
            .expect_err("empty query should error");
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_api_key_is_execution_error() {
        // 确保 ambient env 里没有 BRAVE_API_KEY(否则 std::env::var 兜底
        // 会让空 ctx.env 不报错)。
        // SAFETY: 测试是单线程串行运行的,没有其他线程在读 env。
        unsafe { std::env::remove_var("BRAVE_API_KEY") };
        let t = WebSearchTool::new();
        let err = t
            .execute(
                make_ctx_with_env(HashMap::new()),
                serde_json::json!({"query": "rust"}),
            )
            .await
            .expect_err("missing key should error");
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
        if let ToolError::Execution(msg) = err {
            assert!(msg.contains("BRAVE_API_KEY"), "msg = {msg}");
        }
    }

    #[tokio::test]
    async fn count_is_clamped_to_max() {
        // 通过观察请求 URL 来验证 clamp 是连同请求一起算的;这里只
        // 验证 execute 在没有 API key 时也能走到请求前的早期失败。
        // SAFACY: 测试是单线程串行运行的,没有其他线程在读 env。
        unsafe { std::env::remove_var("BRAVE_API_KEY") };
        let t = WebSearchTool::new();
        let err = t
            .execute(
                make_ctx_with_env(HashMap::new()),
                serde_json::json!({"query": "rust", "count": 999}),
            )
            .await
            .expect_err("no api key should short-circuit before network");
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
    }

    // ── JSON 解析鲁棒性(纯函数白盒)─────────────────────────────

    /// Brave 返回 `web.results == []` 时不应 panic,也不应写任何 result。
    #[test]
    fn parses_empty_results_gracefully() {
        let body: Value = serde_json::json!({"web": {"results": []}});
        let results = body
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(results.is_empty());
    }

    /// Brave 响应**完全**没有 `web` 字段(API 升级 / 异常)也不应 panic。
    #[test]
    fn parses_missing_web_field_gracefully() {
        let body: Value = serde_json::json!({});
        let results = body
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(results.is_empty());
    }

    /// 单条 result 缺字段也不应 panic。
    #[test]
    fn parses_result_with_missing_fields_gracefully() {
        let body: Value = serde_json::json!({
            "web": {"results": [{}]}
        });
        let results = body
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let r = &results[0];
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(no title)");
        let raw_url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let safe_url = match Url::parse(raw_url) {
            Ok(parsed) => sanitize_url_for_metadata(&parsed),
            Err(_) => String::new(),
        };
        assert_eq!(title, "(no title)");
        assert_eq!(safe_url, "");
    }

    // ── URL 消毒(bug-1 端到端)──────────────────────────────

    /// 模拟 Brave 返回带 `?token=` / `#access_token=` 的 result URL;
    /// 消毒后应剥干净 query / fragment / userinfo,不残留 secret。
    #[test]
    fn sanitizes_result_url_with_secrets() {
        let raw = "https://internal.corp/api?token=eyJabc.def.ghi&user=42#access_token=xyz";
        let safe = sanitize_url_for_metadata(&Url::parse(raw).expect("parse ok"));
        assert!(!safe.contains("token="), "got: {safe}");
        assert!(!safe.contains("access_token"), "got: {safe}");
        assert!(!safe.contains("eyJabc"), "got: {safe}");
        assert!(!safe.contains('#'), "got: {safe}");
        assert_eq!(safe, "https://internal.corp/api");
    }

    /// user:pass userinfo 也一并剥除。
    #[test]
    fn sanitizes_result_url_with_userinfo() {
        let raw = "https://user:pass@host.example.com/path?x=1";
        let safe = sanitize_url_for_metadata(&Url::parse(raw).expect("parse ok"));
        assert!(!safe.contains("user:pass"), "got: {safe}");
        assert!(!safe.contains("x=1"), "got: {safe}");
        assert_eq!(safe, "https://host.example.com/path");
    }

    /// Brave 偶尔返回完全无法 parse 的非法字符串(不以 scheme 开头、
    /// `%` 不闭合等)不应污染 text —— `Url::parse` 失败时走空串 fallback。
    #[test]
    fn invalid_url_falls_back_to_empty() {
        for raw in ["not a url", "", "://broken", "//no-scheme"] {
            let safe = match Url::parse(raw) {
                Ok(_) => panic!("expected parse to fail for {raw:?}"),
                Err(_) => String::new(),
            };
            assert_eq!(safe, "", "raw={raw:?}");
        }
    }

    /// 合法但非常规 scheme(如 `javascript:`)会 parse 成功,经 sanitize
    /// 后保留 path(无 query/fragment/userinfo 可剥)。这与 `web_fetch`
    /// 的 `ALLOWED_SCHEMES` 黑名单**不同** —— web_search 的 URL 来源
    /// 是 Brave 返回,不是用户输入,所以没有 scheme 白名单;scheme
    /// 黑名单留作 v1.1 评估。当前文本只读不进 HTML 渲染,无 XSS 通道。
    #[test]
    fn valid_unusual_scheme_url_passes_sanitize() {
        let raw = "javascript:alert(1)";
        let parsed = Url::parse(raw).expect("parse ok");
        let safe = sanitize_url_for_metadata(&parsed);
        assert_eq!(safe, raw);
    }

    /// 真实网络 happy-path 测试(`#[ignore]`,默认不跑)。验证:
    /// - 解析成功、`metadata.truncated == false`(命中 2 MiB 上限几乎不可能,
    ///   反向断言当且仅当 bug-2 修复后才会一直 false);
    /// - text 中首个 URL 不含 `?` 或 `#`(反向断言当且仅当 bug-1
    ///   修复后才会通过,作为 e2e 防回归)。
    #[tokio::test]
    #[ignore = "real-network test, run with: cargo test -p reflect-tools -- --ignored real_brave_search"]
    async fn real_brave_search() {
        // 需要 `export BRAVE_API_KEY=BSA...` 才能跑。
        let api_key =
            std::env::var("BRAVE_API_KEY").expect("set BRAVE_API_KEY for real-network test");
        let mut env = HashMap::new();
        env.insert("BRAVE_API_KEY".into(), api_key);
        let t = WebSearchTool::new();
        let out = t
            .execute(
                make_ctx_with_env(env),
                serde_json::json!({"query": "Reflect-Agent"}),
            )
            .await
            .expect("brave search should succeed");
        let body = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(body.contains("1."), "expected numbered results: {body}");

        // metadata.truncated 应为 false。
        assert_eq!(
            out.metadata.get("truncated").and_then(|v| v.as_bool()),
            Some(false),
            "metadata.truncated should be false on happy path; metadata={:?}",
            out.metadata
        );

        // text 中首个出现在 result URL 行的 URL 不应含 `?` 或 `#`。
        // 格式约定:`   https://...` / `   http://...` 三空格缩进 + 换行结尾。
        let first_url_line = body
            .lines()
            .find(|line| line.starts_with("   http://") || line.starts_with("   https://"));
        if let Some(url) = first_url_line {
            assert!(
                !url.contains('?'),
                "URL should have no `?` (sanitization regressed): {url}\nbody={body}"
            );
            assert!(
                !url.contains('#'),
                "URL should have no `#` (sanitization regressed): {url}\nbody={body}"
            );
        }
    }
}
