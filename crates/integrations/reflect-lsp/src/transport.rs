//! LSP transport —— stdio 上的 JSON-RPC peer。
//!
//! ## 协议
//!
//! LSP spec 要求:`Content-Length: N\r\n\r\n` 头 + 紧跟 N 字节 UTF-8 body,
//! 一次 message 一个 frame。可选 `Content-Type: application/vscode-jsonrpc; charset=utf-8`
//! 头,本实现忽略(读时不解析 Content-Type,写时省略)。
//!
//! ## 分帧层 (`framing` 子模块)
//!
//! 纯函数,无 IO,`Buf` 上增量解析:
/*
```text
buf: [Content-Length: 27\r\n\r\n{"jsonrpc":"2.0", ...]
                ^ headers section    ^ body
```
*/
//! 返回 `FrameState::NeedMore` 表示要等更多字节;`Done(msg)` 表示成功
//! 切出一个完整 message 并消费对应字节。
//!
//! 单元测试覆盖:单 frame / 多 frame 拼接到同一 buffer / 截断 / 超大 body。
//!
//! ## Peer (`LspPeer`)
//!
//! 一个 LSP server 一个 `LspPeer`,内部:
//! - 写 task:独占 child stdin,把 `JsonRpcMessage` 序列化为 frame 写入
//! - 读 task:独占 child stdout,按 framing 切 frame,派发到 inflight oneshot
//!   或 notification 订阅者
//! - inflight: `HashMap<i64, oneshot::Sender<...>>` 把 request id 与响应配对
//!
//! 公共 API:
//! - `request<R>(params) -> Result<R::Result>`:通用请求,泛型由 `lsp_types::request::Request` 提供
//! - `notify<N>(params)`:无 id 通知
//!
//! ## 安全约束
//!
//! - stdio 子进程用 `env_clear()` 清空父进程 env,避免把 `OPENAI_API_KEY`
//!   等敏感凭据泄露到第三方 LSP server 子进程。然后只注入 `HOME`/`PATH` +
//!   用户在 `[lsp_servers.x].env` 显式声明的变量(对齐 `reflect-mcp/transport.rs`)。
//! - `kill_on_drop(true)`:Reflect 主进程 panic 时,子进程一起退出,避免 zombie。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::diagnostics::DiagnosticRegistry;
use crate::error::LspError;

// lsp_types::ServerCapabilities / ServerInfo 显式 import,避免路径穿透到
// transport 公开 API 的返回类型签名。
use lsp_types::{ServerCapabilities, ServerInfo};
use std::str::FromStr;

// ── framing 子模块:纯函数,无 IO ────────────────────────────────────────

/// framing 层返回的状态。
#[derive(Debug)]
pub(crate) enum FrameState {
    /// 还需要更多字节才能切出完整 frame。
    NeedMore,
    /// 成功切出一个完整 message,已从 buffer 消费对应字节。
    Done(Value),
}

/// 把 body 序列化为完整 frame 写入 `buf`。
///
/// 写盘格式:`Content-Length: {N}\r\n\r\n{body_bytes}`(N 是 body 的字节数,
/// 不是字符数)。spec 不要求 Content-Type,本实现省略。
pub(crate) fn write_frame(buf: &mut Vec<u8>, body: &[u8]) {
    // CRLF 严格按 spec:`\r\n` 两个字节。
    // 不用 `write!` 是因为 `Vec<u8>` 不实现 `std::fmt::Write`;手动 push 字节
    // 避免格式化路径上的 UTF-8 中间层。
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(body);
}

/// 尝试从 `buf` 切出一个完整 frame。
///
/// 算法:
/// 1. 找 `\r\n\r\n` 分隔 header 与 body;找不到 → `NeedMore`
/// 2. 在分隔符前找 `Content-Length: N` 头(只解析 `Content-Length`,
///    忽略其他可选头如 `Content-Type`);找不到或解析失败 → `Err` 兜底
/// 3. 检查 `buf.len() >= header_end + N`;不够 → `NeedMore`
/// 4. JSON 解析 body 切片;失败 → `Err`
/// 5. 切片从 `buf` 头部 drain 掉,返回 `Done(parsed)`
pub(crate) fn try_parse_frame(buf: &mut Vec<u8>) -> Result<FrameState, LspError> {
    // 找 header/body 分隔符(CRLF CRLF,4 字节)。
    const SEP: &[u8] = b"\r\n\r\n";
    let Some(sep_pos) = buf.windows(SEP.len()).position(|w| w == SEP) else {
        return Ok(FrameState::NeedMore);
    };
    // header 区在 [0, sep_pos);只解析 Content-Length。
    let header_str = std::str::from_utf8(&buf[..sep_pos])
        .map_err(|e| LspError::Call(format!("non-utf8 header: {e}")))?;
    let content_length = parse_content_length(header_str).ok_or_else(|| {
        LspError::Call(format!(
            "missing or invalid Content-Length in header: {header_str:?}"
        ))
    })?;
    let body_start = sep_pos + SEP.len();
    let body_end = body_start + content_length;
    if buf.len() < body_end {
        // body 还没到齐,等更多字节。
        return Ok(FrameState::NeedMore);
    }
    // 解析 body。
    let body_bytes = &buf[body_start..body_end];
    let value: Value = serde_json::from_slice(body_bytes)
        .map_err(|e| LspError::Call(format!("invalid JSON in LSP frame: {e}")))?;
    // 切掉已消费字节。
    buf.drain(..body_end);
    Ok(FrameState::Done(value))
}

/// 从 header section 解析 `Content-Length: N`。容忍大小写、容忍前导空白、
/// 容忍 header section 内多行(只取第一个匹配的 key)。
fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

// ── LspMessage:发出去的 JSON-RPC 帧结构 ──────────────────────────────────

/// 写到 stdin 的 JSON-RPC 消息(仅 request / notification 两种,本 crate
/// 不主动发 server-to-client request)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JsonRpcMessage {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// `#[serde(default)]` 让反序列化响应(无 `method` 字段)时落到
    /// 空字符串;序列化永远走实际值。
    #[serde(default)]
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ── 内部响应/通知分发状态 ──────────────────────────────────────────────

type InflightTx = oneshot::Sender<Result<Value, LspError>>;

/// stdio LSP peer。所有 method 都是 `&self`,内部状态走 `Arc` + `Mutex`/`mpsc`。
pub struct LspPeer {
    /// 发往 child stdin 的 channel。`writer_tx.send(msg)` 排队;writer task
    /// 是唯一写者,避免多 writer 互相穿插 frame 字节。
    writer_tx: mpsc::Sender<JsonRpcMessage>,
    /// request id 分配器(单调递增)。`Mutex<i64>` 是细粒度锁,只在
    /// `request()` 进入瞬间 lock。
    next_id: Mutex<i64>,
    /// in-flight request 响应 oneshot 表(由 reader task 派发)。
    inflight: Arc<Mutex<HashMap<i64, InflightTx>>>,
    /// shutdown 触发器。`cancel.cancelled()` 后 writer/reader task 退出。
    pub cancel: CancellationToken,
}

impl LspPeer {
    /// 启动 child 进程,搭好 writer/reader task,跑 initialize 握手,
    /// 返回 `(peer, capabilities, server_info)`。
    ///
    /// 失败路径返回 `LspError::Spawn`(子进程 spawn 失败)或
    /// `LspError::Initialize`(握手失败 / framing 启动后立刻 panic 的兜底)。
    pub async fn spawn_with_caps(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        root_uri: Option<&url::Url>,
        initialization_options: Option<&Value>,
        diagnostics: Option<Arc<DiagnosticRegistry>>,
    ) -> Result<(Arc<Self>, ServerCapabilities, Option<ServerInfo>), LspError> {
        // 构造 stdio 子进程。
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        inject_safe_env(&mut cmd, env);
        let mut child: Child = cmd.spawn()?;
        let stdin: ChildStdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Initialize("child stdin missing".to_string()))?;
        let stdout: ChildStdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Initialize("child stdout missing".to_string()))?;
        // 把 child 移进后台 task 持有 — 主进程 cancel 时 task 退出,
        // child drop 触发 `kill_on_drop` 杀子进程。
        let child_cancel = CancellationToken::new();
        let cancel_for_task = child_cancel.clone();
        tokio::spawn(async move {
            let _hold = child;
            cancel_for_task.cancelled().await;
            // _hold 在这里 drop,触发 Child 的 kill_on_drop。
        });
        // writer channel:capacity 32,够 manager / tool 短时突发用。
        let (writer_tx, writer_rx) = mpsc::channel::<JsonRpcMessage>(32);
        // reader 派发表:inflight + 不抛错的 notification sink(本 Phase A
        // 不消费 notification,但仍需要个 drop-or-ignore 通道避免 reader
        // task 在收到 notification 时无法分发)。
        let inflight: Arc<Mutex<HashMap<i64, InflightTx>>> = Arc::new(Mutex::new(HashMap::new()));
        // writer task(写任务)
        tokio::spawn(writer_loop(stdin, writer_rx, child_cancel.clone()));
        // reader task(读任务)
        tokio::spawn(reader_loop(
            BufReader::new(stdout),
            inflight.clone(),
            child_cancel.clone(),
            diagnostics,
        ));
        // 构造 peer。
        let peer = Arc::new(Self {
            writer_tx,
            next_id: Mutex::new(1),
            inflight,
            cancel: child_cancel,
        });
        // initialize 握手(异步,等 server 回应)。
        let init_result = peer
            .initialize(root_uri, initialization_options)
            .await
            .inspect_err(|_| {
                // 握手失败 → cancel peer(子进程随之 kill)。
                peer.cancel.cancel();
            })?;
        // 解析 response:取 serverInfo + capabilities。
        let server_info: Option<ServerInfo> = init_result
            .get("serverInfo")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let capabilities: ServerCapabilities = init_result
            .get("capabilities")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        tracing::debug!(
            server = ?server_info.as_ref().map(|s| &s.name),
            "LSP initialize handshake complete"
        );
        Ok((peer, capabilities, server_info))
    }

    /// 测试用 stub:不接 child,仅返回空 inflight + 默认 cancel。
    ///
    /// 文档同步 (`document.rs`) 单测用此构造 `LspClientInner` 验证
    /// `close_all` / `docs` map 行为,**不**调 peer 上的 request/notify
    /// 路径(那些需要真 child)。
    #[cfg(test)]
    pub fn stub_for_tests() -> Self {
        let (writer_tx, _writer_rx) = mpsc::channel::<JsonRpcMessage>(1);
        Self {
            writer_tx,
            next_id: Mutex::new(1),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            cancel: CancellationToken::new(),
        }
    }

    /// `initialize` 握手(必须最先调用)。
    ///
    /// 走 `request::<lsp_types::request::Initialize>` 路径,返回完整
    /// `InitializeResult`(`ServerInfo` + `ServerCapabilities`)。`shutdown`
    /// + `exit` 由 `LspConnectionManager::stop_server` 在结束时发。
    fn initialize(
        &self,
        root_uri: Option<&url::Url>,
        initialization_options: Option<&Value>,
    ) -> impl std::future::Future<Output = Result<Value, LspError>> {
        let params = lsp_types::InitializeParams {
            // LSP spec 要求 process id,0 = 客户端不跟踪子进程 PId。
            process_id: Some(std::process::id()),
            client_info: Some(lsp_types::ClientInfo {
                name: "reflect".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            locale: None,
            // InitializeParams.root_uri 已 deprecated,新版走 workspace_folders;
            // 我们仍填 root_uri(简单且 Phase A 兼容老 server)。
            #[allow(deprecated)]
            root_uri: root_uri.and_then(|u| lsp_types::Uri::from_str(u.as_str()).ok()),
            capabilities: lsp_types::ClientCapabilities {
                workspace: None,
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        did_save: Some(false),
                    }),
                    ..Default::default()
                }),
                window: None,
                general: None,
                notebook_document: None,
                ..Default::default()
            },
            initialization_options: initialization_options.cloned(),
            trace: None,
            workspace_folders: None,
            ..Default::default()
        };
        async move {
            use lsp_types::request::Initialize;
            let raw: lsp_types::InitializeResult = self.request::<Initialize>(params).await?;
            serde_json::to_value(raw)
                .map_err(|e| LspError::Call(format!("re-encode init result: {e}")))
        }
    }

    /// 通用 request:分配 id,注册 oneshot,发 frame,等结果。
    ///
    /// 泛型 `R: lsp_types::request::Request` 由调用方提供
    /// (`lsp_types::request::GotoDefinition`、`References` 等),
    /// 这里用 `R::METHOD` 拿方法名、`R::Params` 做参数类型、
    /// `R::Result` 做响应类型。
    pub async fn request<R>(&self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
        R::Result: serde::de::DeserializeOwned,
    {
        // 分配 id。
        let id = {
            let mut guard = self.next_id.lock();
            let id = *guard;
            *guard = id.wrapping_add(1);
            id
        };
        let (tx, rx) = oneshot::channel::<Result<Value, LspError>>();
        self.inflight.lock().insert(id, tx);
        // 构造消息。
        let msg = JsonRpcMessage {
            jsonrpc: "2.0",
            id: Some(id),
            method: R::METHOD.to_string(),
            params: Some(
                serde_json::to_value(&params)
                    .map_err(|e| LspError::Call(format!("encode params: {e}")))?,
            ),
        };
        // 发到 writer(走 channel 不阻塞在这里;channel 满时 back-pressure 等 writer)。
        if self.writer_tx.send(msg).await.is_err() {
            // writer task 已退出,反注册 inflight 避免 reader 持有一个永远等不到的 oneshot。
            self.inflight.lock().remove(&id);
            return Err(LspError::Call("lsp peer writer closed".to_string()));
        }
        // 等响应或 cancel。
        let value = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                self.inflight.lock().remove(&id);
                return Err(LspError::Cancelled);
            }
            r = rx => r.map_err(|_| LspError::Call("lsp peer response channel closed".to_string()))?,
        };
        let value = value?;
        // 解析响应:有 "result" → Ok, 有 "error" → ServerError。
        if let Some(err) = value.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0) as i32;
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            return Err(LspError::ServerError { code, message });
        }
        let result_value = value
            .get("result")
            .cloned()
            .ok_or_else(|| LspError::Call("lsp response missing 'result'".to_string()))?;
        serde_json::from_value(result_value)
            .map_err(|e| LspError::Call(format!("decode lsp result: {e}")))
    }

    /// 通用 notification:无 id,server 不回响应。
    pub async fn notify<N>(&self, params: N::Params) -> Result<(), LspError>
    where
        N: lsp_types::notification::Notification,
        N::Params: Serialize,
    {
        let msg = JsonRpcMessage {
            jsonrpc: "2.0",
            id: None,
            method: N::METHOD.to_string(),
            params: Some(
                serde_json::to_value(&params)
                    .map_err(|e| LspError::Call(format!("encode notify params: {e}")))?,
            ),
        };
        self.writer_tx
            .send(msg)
            .await
            .map_err(|_| LspError::Call("lsp peer writer closed".to_string()))?;
        Ok(())
    }
}

// ── writer task:从 channel 取消息 → 写 frame 到 stdin ──────────────────

async fn writer_loop(
    mut stdin: ChildStdin,
    mut rx: mpsc::Receiver<JsonRpcMessage>,
    cancel: CancellationToken,
) {
    let mut buf = Vec::with_capacity(512);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // 关 stdin 让 server 收到 EOF → 走完 shutdown 协议。
                let _ = stdin.shutdown().await;
                break;
            }
            maybe = rx.recv() => {
                let Some(msg) = maybe else { break };
                buf.clear();
                let body = match serde_json::to_vec(&msg) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = %e, "lsp writer: failed to encode message");
                        continue;
                    }
                };
                write_frame(&mut buf, &body);
                if let Err(e) = stdin.write_all(&buf).await {
                    tracing::warn!(error = %e, "lsp writer: stdin write failed; exiting");
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    tracing::warn!(error = %e, "lsp writer: stdin flush failed");
                    // flush 失败通常意味着子进程已死,break 让 outer 清理。
                    break;
                }
            }
        }
    }
}

// ── reader task:从 stdout 读 frame → 派发到 inflight ──────────────────

async fn reader_loop(
    mut stdout: BufReader<ChildStdout>,
    inflight: Arc<Mutex<HashMap<i64, InflightTx>>>,
    cancel: CancellationToken,
    diagnostics: Option<Arc<DiagnosticRegistry>>,
) {
    // 用一个累积 buffer 装 stdin 字节,逐次调 try_parse_frame 切 frame。
    // 当 client 是单线程(LSP request 顺序)时,这里可以一行一解析;但
    // reader task 拿的是 raw bytes,我们仍按"累积 buffer + 切 frame"模式
    // 实现以正确处理"body 跨 read 边界"的情况(实际很少见但 spec 允许)。
    let mut buf = Vec::with_capacity(4096);
    let mut raw_chunk = [0u8; 4096];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            // 拿一次 read;EOF → 子进程退出了,推 cancel + 通知 inflight。
            read = stdout.read(&mut raw_chunk) => {
                match read {
                    Ok(0) => {
                        // EOF:把所有 inflight oneshot 标 Cancelled。
                        let mut guard = inflight.lock();
                        for (_, tx) in guard.drain() {
                            let _ = tx.send(Err(LspError::Cancelled));
                        }
                        cancel.cancel();
                        break;
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&raw_chunk[..n]);
                        // 持续切 frame 直到 buffer 切干净。
                        loop {
                            match try_parse_frame(&mut buf) {
                                Ok(FrameState::NeedMore) => break,
                                Ok(FrameState::Done(value)) => {
                                    // 派发:有 id → inflight;无 id → 忽略(本 Phase A
                                    // 不实现 notification 处理,典型 publishDiagnostics
                                    // 落到这里会被丢,Phase B 接)。
                                    let id = value.get("id").and_then(Value::as_i64);
                                    match id {
                                        Some(id) => {
                                            let mut guard = inflight.lock();
                                            if let Some(tx) = guard.remove(&id) {
                                                drop(guard);
                                                let _ = tx.send(Ok(value));
                                            }
                                            // 没有 inflight(id 已超时被 cancel 移除)→ 静默丢
                                        }
                                        None => {
                                            let method = value
                                                .get("method")
                                                .and_then(serde_json::Value::as_str)
                                                .unwrap_or("?");
                                            if let Some(ref reg) = diagnostics
                                                && let Some(params) = value.get("params")
                                            {
                                                reg.ingest_notification(method, params);
                                            }
                                            tracing::trace!(method = %method, "lsp notification received");
                                        }
                                    }
                                }
                                Err(e) => {
                                    // framing 失败:协议层错误,记 warn + 跳过这个 frame
                                    // 不容易恢复(不一定是 body 解析失败还是 header 截断);
                                    // 选择 drain 一字节让循环继续,避免死循环。
                                    tracing::warn!(error = %e, "lsp reader: invalid frame");
                                    if !buf.is_empty() {
                                        buf.remove(0);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "lsp reader: stdout read failed");
                        cancel.cancel();
                        break;
                    }
                }
            }
        }
    }
}

// ── env 注入 helper ────────────────────────────────────────────────────

/// 安全注入 env:与 `reflect-mcp::transport::inject_safe_env` 完全对称。
///
/// `env_clear()` 后只加 `HOME`/`PATH` + 用户显式 env,避免父进程的
/// `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` 泄漏到第三方 LSP server。
fn inject_safe_env(cmd: &mut Command, user_env: &HashMap<String, String>) {
    cmd.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    } else if let Some(dir) = std::env::temp_dir().to_str() {
        cmd.env("HOME", dir);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in user_env {
        cmd.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── framing 单测 ──────────────────────────────────────────────────

    #[test]
    fn write_frame_emits_correct_header_and_body() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"foo"}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, body);
        let expected_header = format!("Content-Length: {}\r\n\r\n", body.len());
        let expected = format!("{expected_header}{}", std::str::from_utf8(body).unwrap());
        assert_eq!(buf, expected.as_bytes());
    }

    #[test]
    fn parse_single_complete_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, body);
        let parsed = try_parse_frame(&mut buf).expect("parse ok");
        match parsed {
            FrameState::Done(v) => assert_eq!(v["id"], 1),
            FrameState::NeedMore => panic!("expected Done"),
        }
        assert!(buf.is_empty(), "frame should be drained");
    }

    #[test]
    fn parse_two_frames_back_to_back() {
        let body1 = br#"{"jsonrpc":"2.0","id":1,"method":"a"}"#;
        let body2 = br#"{"jsonrpc":"2.0","id":2,"method":"b"}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, body1);
        write_frame(&mut buf, body2);
        // 第一个 frame
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::Done(v) => assert_eq!(v["id"], 1),
            FrameState::NeedMore => panic!("expected first Done"),
        }
        // 第二个 frame
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::Done(v) => assert_eq!(v["id"], 2),
            FrameState::NeedMore => panic!("expected second Done"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_truncated_frame_returns_need_more() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, body);
        // 截掉 body 最后 4 字节
        buf.truncate(buf.len() - 4);
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::NeedMore => {}
            FrameState::Done(_) => panic!("expected NeedMore"),
        }
        // 补回缺失的 4 字节:`"ok"}`(body 末尾 4 字符)
        buf.extend_from_slice(b"ok\"}");
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::Done(v) => assert_eq!(v["result"], "ok"),
            FrameState::NeedMore => panic!("expected Done after resumption"),
        }
    }

    #[test]
    fn parse_missing_header_returns_need_more() {
        let mut buf = b"Content-Length: 10\r\n".to_vec();
        // body 还没到
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::NeedMore => {}
            FrameState::Done(_) => panic!("expected NeedMore"),
        }
        // CRLF 还没到(header section 未结束)
        let mut buf2 = b"Content-Length: 10\r".to_vec();
        match try_parse_frame(&mut buf2).unwrap() {
            FrameState::NeedMore => {}
            FrameState::Done(_) => panic!("expected NeedMore"),
        }
    }

    #[test]
    fn parse_missing_content_length_header_errors() {
        let mut buf = b"X-Custom: foo\r\n\r\n{}".to_vec();
        let r = try_parse_frame(&mut buf);
        assert!(matches!(r, Err(LspError::Call(_))));
    }

    #[test]
    fn parse_invalid_json_body_errors() {
        let body = b"not json";
        let mut buf = Vec::new();
        write_frame(&mut buf, body);
        let r = try_parse_frame(&mut buf);
        assert!(matches!(r, Err(LspError::Call(_))));
    }

    #[test]
    fn large_body_round_trip() {
        // 1 MB body,验证 usize 转换没截断。用大 JSON 字符串作为 body。
        let large_str = "x".repeat(1024 * 1024);
        let body = serde_json::to_vec(&serde_json::Value::String(large_str.clone())).unwrap();
        let mut buf = Vec::new();
        write_frame(&mut buf, &body);
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::Done(v) => {
                assert_eq!(v.as_str().unwrap().len(), large_str.len());
            }
            FrameState::NeedMore => panic!("expected Done"),
        }
    }

    #[test]
    fn content_length_case_insensitive() {
        let body = b"{}";
        let mut buf = b"content-length: 2\r\n\r\n".to_vec();
        buf.extend_from_slice(body);
        match try_parse_frame(&mut buf).unwrap() {
            FrameState::Done(_) => {}
            FrameState::NeedMore => panic!("expected Done"),
        }
    }

    // ── env 注入单测(对齐 reflect-mcp) ──────────────────────────────

    #[test]
    fn inject_safe_env_drops_parent_env() {
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
        assert!(
            !envs.iter().any(|(k, _)| *k == "OPENAI_API_KEY"),
            "OPENAI_API_KEY leaked: {envs:?}"
        );
        assert!(envs.iter().any(|(k, _)| *k == "MY_KEY"));
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    // ── JsonRpcMessage 序列化 ───────────────────────────────────────

    #[test]
    fn jsonrpc_message_serialization() {
        let m = JsonRpcMessage {
            jsonrpc: "2.0",
            id: Some(1),
            method: "textDocument/definition".to_string(),
            params: Some(
                json!({"textDocument": {"uri": "file:///x.rs"}, "position": {"line": 0, "character": 0}}),
            ),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":1"#));
        assert!(s.contains(r#""method":"textDocument/definition""#));
        // 反向:server response
        let r: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        assert_eq!(r.id, Some(1));
        assert_eq!(r.method, "");
        // 反向:notification(无 id)
        let n: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}}"#,
        )
        .unwrap();
        assert!(n.id.is_none());
        assert_eq!(n.method, "textDocument/publishDiagnostics");
    }
}
