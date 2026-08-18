//! MCP transport 构造:stdio、streamable-http 与 legacy SSE。
//!
//! ## 安全约束
//!
//! - stdio 子进程用 `env_clear()` 清空父进程 env,避免把
//!   `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` 等敏感凭据泄露到
//!   第三方 MCP server 子进程。然后只注入:
//!   - `HOME`(很多 MCP server 找配置文件需要)
//!   - `PATH`(找子命令需要)
//!   - 用户在 `[mcp_servers.x].env` 显式声明的变量
//! - `kill_on_drop(true)`:Reflect 主进程 panic 时,子进程一起退出,
//!   避免 zombie。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use rmcp::transport::{
    TokioChildProcess,
    streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig},
};
use tokio::process::Command;

use crate::McpError;
use crate::config::McpTransport;

/// 已构造好的 transport 与一个 `RunningService<RoleClient, ()>`
/// —— 本 crate 不暴露 rmcp 类型穿透到 caller;`manager.rs` 直接消费。
pub struct McpTransportHandle {
    /// `Peer<RoleClient>`,由 manager 与 tool adapter 共享。
    pub peer: Arc<rmcp::service::Peer<rmcp::RoleClient>>,
    /// shutdown 时打断 in-flight call。`()` handler 没实现 ping/anything,
    /// 我们用 `CancellationToken` 走双 cancel(manager shutdown + ctx.cancel)。
    pub cancel: tokio_util::sync::CancellationToken,
}

/// 根据 transport 类型分发构造。
pub async fn build(
    transport: McpTransport,
    command: Option<&str>,
    args: &[String],
    env: &HashMap<String, String>,
    url: Option<&str>,
    headers: &HashMap<String, String>,
) -> Result<McpTransportHandle, McpError> {
    match transport {
        McpTransport::Stdio => build_stdio(command, args, env).await,
        McpTransport::Http => build_http(url, headers).await,
        McpTransport::Sse => build_sse(url, headers).await,
    }
}

/// 构造 stdio transport。
///
/// 完整流程:
/// 1. 构造 `tokio::process::Command`,设 `stdin/stdout=Stdio::piped()`,
///    `stderr=Stdio::inherit()`(让 MCP server 自己的 banner 可见);
/// 2. `env_clear()` 后注入 `HOME`/`PATH` + 用户 env;
/// 3. spawn 出 `tokio::process::Child`;
/// 4. 用 `TokioChildProcess` 包装 `(read, write)`;
/// 5. `().serve(transport)` 跑 initialize 握手。
pub async fn build_stdio(
    command: Option<&str>,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<McpTransportHandle, McpError> {
    let cmd_str =
        command.ok_or_else(|| McpError::ConfigInvalid("stdio requires `command`".to_string()))?;
    // 展开 command 与 args 里的环境变量($VAR / ${VAR})。
    // 用户常在 config 写 `args = ["$HOME"]` 指代家目录;不展开的话子进程会收到
    // 字面量 "$HOME",拼出非法路径(`<cwd>/$HOME`)。展开发生在 spawn 前,
    // 用的是注入后的 env(含 HOME/PATH + 用户声明的 env)。
    let cmd_str = expand_env_vars(cmd_str, env);
    let expanded_args: Vec<String> = args.iter().map(|a| expand_env_vars(a, env)).collect();
    let mut cmd = Command::new(&cmd_str);
    cmd.args(&expanded_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Reflect 主进程 panic / cancel 时,kill 子进程避免 zombie。
        // tokio::process::Command::kill_on_drop 自 1.21 起稳定。
        .kill_on_drop(true);
    inject_safe_env(&mut cmd, env);
    let transport = TokioChildProcess::new(cmd).map_err(McpError::Spawn)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let running = rmcp::service::ServiceExt::serve((), transport)
        .await
        .map_err(|e| McpError::Initialize(format!("stdio serve: {e}")))?;
    // Peer<R> derives Clone (rmcp 1.7);抽出 owned Peer 用 Arc 包好共享。
    let peer = Arc::new(running.peer().clone());
    // 把 `running` 移进后台 task 持有 —— DropGuard(在 RunningService 内部)会一直
    // 跟着 task 存活,从而 transport / 子进程保持打开。
    // `cancel.cancelled()` 触发后 task 才让 `running` drop,此时 DropGuard 取消
    // 内部 cancel token → TokioChildProcess 关闭 pipes → kill_on_drop 杀子进程。
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let _hold = running; // 关键:running 必须活过 `list_all_tools` 等调用。
        cancel_clone.cancelled().await;
        // `_hold` 在这里 drop,触发 RunningService DropGuard。
    });
    Ok(McpTransportHandle { peer, cancel })
}

/// 构造 streamable-http transport。
///
/// `headers["Authorization"]` 自动剥离到 `auth_header`,其余 header
/// 透传到 `custom_headers`。
pub async fn build_http(
    url: Option<&str>,
    headers: &HashMap<String, String>,
) -> Result<McpTransportHandle, McpError> {
    let url_str = url.ok_or_else(|| McpError::ConfigInvalid("http requires `url`".to_string()))?;
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url_str);
    let mut custom_headers: std::collections::HashMap<
        reqwest::header::HeaderName,
        reqwest::header::HeaderValue,
    > = std::collections::HashMap::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("authorization") {
            // 去掉 "Bearer " 前缀,rmcp 内部会自己加回去。
            let token = v.strip_prefix("Bearer ").unwrap_or(v.as_str()).to_string();
            cfg = cfg.auth_header(token);
        } else {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::ConfigInvalid(format!("bad header name {k:?}: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| McpError::ConfigInvalid(format!("bad header value {k:?}: {e}")))?;
            custom_headers.insert(name, value);
        }
    }
    cfg = cfg.custom_headers(custom_headers);
    let reqwest_client = reqwest::Client::new();
    let transport = StreamableHttpClientTransport::with_client(reqwest_client, cfg);
    let cancel = tokio_util::sync::CancellationToken::new();
    let running = rmcp::service::ServiceExt::serve((), transport)
        .await
        .map_err(|e| McpError::Initialize(format!("http serve: {e}")))?;
    let peer = Arc::new(running.peer().clone());
    // 同 stdio:running 移进后台 task 持有,等 cancel 触发再 drop(关 HTTP 连接)。
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let _hold = running;
        cancel_clone.cancelled().await;
    });
    Ok(McpTransportHandle { peer, cancel })
}

/// 构造 legacy MCP HTTP+SSE transport。
///
/// v1+ 阶段:legacy 纯 SSE endpoint 协议与 streamable-http 在 wire 层相近,
/// 此处复用 `StreamableHttpClientTransport`(内部走 SSE 长连接)。若服务端
/// 仅支持旧式 `endpoint` 事件握手,请改用 `type = "stdio"` 或升级 server。
pub async fn build_sse(
    url: Option<&str>,
    headers: &HashMap<String, String>,
) -> Result<McpTransportHandle, McpError> {
    tracing::debug!("mcp transport=sse: delegating to streamable-http SSE client");
    build_http(url, headers).await
}

/// 安全注入 env:`env_clear()` 后只加 `HOME`/`PATH` + 用户 env。
///
/// `HOME` 兜底 `tmpdir()` 以保证 stdio 子进程能找到配置文件。
/// `PATH` 直接复用父进程当前值(进程级 PATH 一般无害)。
fn inject_safe_env(cmd: &mut Command, user_env: &HashMap<String, String>) {
    cmd.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    } else if let Some(dir) = std::env::temp_dir().to_str() {
        // tmpdir() 退路 —— 测试 / sandbox 环境可能 HOME 未设。
        cmd.env("HOME", dir);
    }
    // PATH: 优先父进程,无则空(子进程可执行性会立即失败,易诊断)。
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in user_env {
        cmd.env(k, v);
    }
}

/// 展开 string 里的环境变量 `$VAR` / `${VAR}` 为对应值。
///
/// 查询顺序:`user_env`(用户在 `[mcp_servers.x].env` 显式声明的)优先,
/// 其次父进程 `std::env::var`。未定义的变量替换为空字符串(与 shell 行为
/// 一致:`echo $UNDEFINED` 输出空)。
///
/// 仅做变量展开,不支持命令替换/通配符/管道等 shell 特性 —— MCP server
/// 的 command/args 不该有 shell 元语义,展开 `$VAR` 已覆盖 99% 用例
/// (典型:`$HOME`、`$PROJECT_DIR`)。
fn expand_env_vars(s: &str, user_env: &HashMap<String, String>) -> String {
    // 没有 $ 直接返回,避免无谓分配。
    if !s.contains('$') {
        return s.to_string();
    }
    let lookup = |name: &str| -> String {
        user_env
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .unwrap_or_default()
    };
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            // ${VAR} 形式
            if bytes[i + 1] == b'{'
                && let Some(close) = s[i + 2..].find('}')
            {
                let name = &s[i + 2..i + 2 + close];
                out.push_str(&lookup(name));
                i = i + 2 + close + 1;
                continue;
            }
            // $VAR 形式(字母/数字/下划线)
            let rest = &s[i + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            if end > 0 {
                let name = &rest[..end];
                out.push_str(&lookup(name));
                i = i + 1 + end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `inject_safe_env` 不会泄漏父进程的 API key 到子进程。
    /// 通过观察最终 child 的 env 间接断言 —— 我们只验证函数本身
    /// 调用了 `env_clear` (不会保留父进程 `OPENAI_API_KEY`)。
    #[test]
    fn mcp_transport_sse_variant_exists() {
        assert_eq!(
            McpTransport::Sse,
            McpTransport::from(reflect_config::McpTransport::Sse)
        );
    }

    #[test]
    fn inject_safe_env_drops_parent_env() {
        // SAFETY: 测试中串行修改 env 在单测下可控。
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-leak-test");
            std::env::set_var("HOME", "/tmp/test-home");
            std::env::set_var("PATH", "/usr/bin");
        }
        let mut cmd = Command::new("echo");
        let mut user_env = HashMap::new();
        user_env.insert("MY_KEY".to_string(), "v".to_string());
        inject_safe_env(&mut cmd, &user_env);
        let envs: Vec<(&str, &str)> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| v.map(|vv| (k.to_str().unwrap_or(""), vv.to_str().unwrap_or(""))))
            .collect();
        // 父进程的 OPENAI_API_KEY 一定不在;用户注入的 MY_KEY 在。
        assert!(
            !envs.iter().any(|(k, _)| *k == "OPENAI_API_KEY"),
            "OPENAI_API_KEY leaked into child env: {envs:?}"
        );
        assert!(envs.iter().any(|(k, _)| *k == "MY_KEY"));
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[test]
    fn expand_env_vars_dollar_brace_form() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_vars("${FOO}/x", &env), "bar/x");
        assert_eq!(expand_env_vars("pre-${FOO}-post", &env), "pre-bar-post");
    }

    #[test]
    fn expand_env_vars_bare_dollar_form() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/Users/test".to_string());
        assert_eq!(expand_env_vars("$HOME/sub", &env), "/Users/test/sub");
        assert_eq!(expand_env_vars("--root $HOME", &env), "--root /Users/test");
    }

    #[test]
    fn expand_env_vars_undefined_becomes_empty() {
        let env = HashMap::new();
        // 未定义变量 → 空字符串(与 shell 一致)。
        assert_eq!(expand_env_vars("$UNDEFINED_VAR/x", &env), "/x");
        assert_eq!(expand_env_vars("${UNDEFINED_VAR}", &env), "");
    }

    #[test]
    fn expand_env_vars_no_dollar_passthrough() {
        let env = HashMap::new();
        // 无 $ 直接返回原值(不分配)。
        assert_eq!(expand_env_vars("plain-string", &env), "plain-string");
        assert_eq!(expand_env_vars("/usr/bin/npx", &env), "/usr/bin/npx");
    }

    #[test]
    fn expand_env_vars_user_env_overrides_parent() {
        // user_env 优先级高于父进程 std::env::var。
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/custom/path".to_string());
        assert_eq!(expand_env_vars("$PATH", &env), "/custom/path");
    }
}
