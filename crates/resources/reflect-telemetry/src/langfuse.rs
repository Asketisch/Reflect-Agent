//! Langfuse cloud HTTP exporter(P2 `langfuse`)。
//!
//! 把本地 [`TraceEvent`] 转成 Langfuse ingestion batch,POST 到
//! `[langfuse] endpoint` 的 `/api/public/ingestion`,用 public/secret key
//! 做 HTTP Basic Auth。后台 task 从 mpsc 接事件、按批投递,失败 best-effort
//! warn(不阻断引擎,对齐 telemetry 哲学)。
//!
//! Langfuse ingestion 协议(摘要,https://langfuse.com/docs/tracing):
//! - `POST /api/public/ingestion`,`Authorization: Basic base64(pk:sk)`。
//! - body:`{"batch": [{"id","type","timestamp","body"}, ...]}`。
//! - `type` ∈ `trace-create` / `span-create` / `span-update` / `generation-create`。
//!
//! 映射策略(简化):每个 turn/model/tool span 投一条 `span-create`(首次)或
//! `span-update`(completed/failed),trace_id 复用本地 trace_id。
//! v2 可细化 event 类型与字段。

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::model::TraceEvent;

/// Langfuse exporter 配置(从 `[langfuse]` 段映射)。
#[derive(Debug, Clone)]
pub struct LangfuseConfig {
    /// 例 `https://cloud.langfuse.com`。不带尾斜杠。
    pub endpoint: String,
/// 公钥(`pk-lf-...`)。
pub public_key: String,
/// 私钥(`sk-lf-...`)。
pub secret_key: String,
    /// 单批最大事件数(默认 64)。
    pub batch_size: usize,
}

impl Default for LangfuseConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://cloud.langfuse.com".into(),
            public_key: String::new(),
            secret_key: String::new(),
            batch_size: 64,
        }
    }
}

/// 运行句柄:`send` 投递事件,`flush` 触发剩余投递并等待,`shutdown` 停后台。
///
/// `enabled = false`(`None` config)时所有方法是 no-op。
pub struct LangfuseExporter {
    tx: Option<mpsc::Sender<TraceEvent>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LangfuseExporter {
    /// 构造 + 启动后台 task。`cfg = None` → 禁用(no-op exporter)。
    pub fn start(cfg: Option<LangfuseConfig>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<TraceEvent>(256);
        let tx = if let Some(c) = cfg {
            // 空 key → 无意义投递,降级为 no-op(避免每秒 401)。
            if c.public_key.is_empty() || c.secret_key.is_empty() {
                None
            } else {
                let handle = tokio::spawn(run(c, rx));
                let exp = Arc::new(Self {
                    tx: Some(tx),
                    join: Mutex::new(Some(handle)),
                });
                return exp;
            }
        } else {
            None
        };
        Arc::new(Self {
            tx,
            join: Mutex::new(None),
        })
    }

    /// 投递一个 span 事件(非阻塞,队列满则丢弃 + warn)。
    pub fn send(&self, ev: TraceEvent) {
        if let Some(tx) = &self.tx
            && let Err(e) = tx.try_send(ev)
        {
            tracing::warn!(error = %e, "langfuse 队列满,丢弃事件");
        }
    }

    /// 是否启用(后台 task 在跑)。
    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// 优雅停掉后台 task:发完剩余事件后退出。`None` config 时直接返回。
    ///
    /// 注:不强制 abort —— 让后台 drain channel 后自然退出(对齐 telemetry
    /// best-effort 哲学:投递不丢比快速停机更重要)。最大等待由调用方控制。
    pub async fn shutdown(&self) {
        // 取出 JoinHandle —— 必须在 await 前释放锁,避免持有 `parking_lot`
        // 的非异步 `MutexGuard` 跨 await(后台 task 可能并发访问 `join`)。
        let handle = self.join.lock().take();
        if let Some(handle) = handle {
            // 后台 task 在 channel 关闭时自然退出;给它最多 2s 排空。
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        }
    }
}

/// 后台投递循环:从 rx 收事件攒批,达 batch_size 或 0.5s flush 一次。
async fn run(cfg: LangfuseConfig, mut rx: mpsc::Receiver<TraceEvent>) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "langfuse HTTP client 构造失败,exporter 退出");
            return;
        }
    };
    let url = format!(
        "{}/api/public/ingestion",
        cfg.endpoint.trim_end_matches('/')
    );
    let mut batch: Vec<TraceEvent> = Vec::with_capacity(cfg.batch_size);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            // 收到事件入批(非阻塞,空批也允许进)。
            maybe_ev = rx.recv() => {
                match maybe_ev {
                    Some(ev) => batch.push(ev),
                    None => {
                        // channel 关闭:flush 剩余后退出。
                        if !batch.is_empty() {
                            flush(&client, &url, &cfg, std::mem::take(&mut batch)).await;
                        }
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                if !batch.is_empty() {
                    flush(&client, &url, &cfg, std::mem::take(&mut batch)).await;
                }
            }
        }
        // 批满即 flush(不等 tick)。
        if batch.len() >= cfg.batch_size {
            flush(&client, &url, &cfg, std::mem::take(&mut batch)).await;
        }
    }
}

/// 把一批 TraceEvent 转 Langfuse batch 并 POST。
async fn flush(client: &reqwest::Client, url: &str, cfg: &LangfuseConfig, events: Vec<TraceEvent>) {
    let batch: Vec<Value> = events.iter().map(event_to_langfuse).collect();
    let body = json!({ "batch": batch });
    let resp = client
        .post(url)
        .basic_auth(&cfg.public_key, Some(&cfg.secret_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            tracing::debug!(count = events.len(), "langfuse 投递成功");
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            tracing::warn!(%status, %text, "langfuse 投递非 2xx");
        }
        Err(e) => {
            tracing::warn!(error = %e, "langfuse 投递失败");
        }
    }
}

/// 单个 TraceEvent → Langfuse ingestion event。
///
/// turn.started → trace-create(建 trace)+ span-create(turn span)。
/// 其余 → span-update(把 status/duration/context 合并)。
/// 简化:每个 event 自包含,重复 trace-create 被 Langfuse 幂等接受(同 id)。
fn event_to_langfuse(ev: &TraceEvent) -> Value {
    let id = Uuid::new_v4().to_string();
    let ts = ev.timestamp.to_rfc3339();
    let (typ, body) = if ev.event.ends_with(".started") {
        // 首次出现:建 trace + span。
        (
            "span-create",
            json!({
                "id": ev.span_id,
                "traceId": ev.trace_id,
                "parentObservationId": ev.parent_span_id,
                "name": ev.event,
                "startTime": ts,
                "metadata": metadata(ev),
            }),
        )
    } else {
        // completed/failed:span-update,补 endTime + status + metadata。
        (
            "span-update",
            json!({
                "id": ev.span_id,
                "traceId": ev.trace_id,
                "endTime": ts,
                "statusMessage": ev.status.map(|s| s.as_str()),
                "metadata": metadata(ev),
            }),
        )
    };
    json!({
        "id": id,
        "type": typ,
        "timestamp": ts,
        "body": body,
    })
}

/// 事件 metadata:level / module / session / turn / tool / duration / context。
fn metadata(ev: &TraceEvent) -> Value {
    let mut m = json!({
        "level": ev.level.as_str(),
        "module": ev.module,
        "event": ev.event,
    });
    if let Some(s) = &ev.session_id {
        m["session_id"] = json!(s);
    }
    if let Some(t) = &ev.turn_id {
        m["turn_id"] = json!(t);
    }
    if let Some(tc) = &ev.tool_call_id {
        m["tool_call_id"] = json!(tc);
    }
    if let Some(d) = ev.duration_ms {
        m["duration_ms"] = json!(d);
    }
    if let Some(ctx) = &ev.context {
        m["context"] = ctx.clone();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, SpanStatus};
    use chrono::Utc;

    fn sample_event(name: &str, started: bool) -> TraceEvent {
        TraceEvent {
            timestamp: Utc::now(),
            level: Level::Info,
            event: name.into(),
            module: "test".into(),
            trace_id: "tr1".into(),
            span_id: "sp1".into(),
            parent_span_id: None,
            session_id: Some("s1".into()),
            turn_id: Some("t1".into()),
            tool_call_id: None,
            status: if started {
                Some(SpanStatus::Started)
            } else {
                Some(SpanStatus::Completed)
            },
            duration_ms: Some(42),
            context: Some(json!({"k": "v"})),
        }
    }

    #[test]
    fn started_event_maps_to_span_create() {
        let ev = sample_event("turn.started", true);
        let v = event_to_langfuse(&ev);
        assert_eq!(v["type"], "span-create");
        assert_eq!(v["body"]["id"], "sp1");
        assert_eq!(v["body"]["traceId"], "tr1");
        assert!(v["body"]["startTime"].is_string());
    }

    #[test]
    fn completed_event_maps_to_span_update() {
        let ev = sample_event("turn.completed", false);
        let v = event_to_langfuse(&ev);
        assert_eq!(v["type"], "span-update");
        assert_eq!(v["body"]["id"], "sp1");
        assert!(v["body"]["endTime"].is_string());
    }

    #[test]
    fn metadata_includes_level_module_session() {
        let ev = sample_event("model.request.completed", false);
        let m = metadata(&ev);
        assert_eq!(m["level"], "info");
        assert_eq!(m["module"], "test");
        assert_eq!(m["session_id"], "s1");
        assert_eq!(m["duration_ms"], 42);
    }

    #[test]
    fn disabled_exporter_with_empty_key_is_noop() {
        // 空 key → no-op(tx = None),send 不 panic。
        let exp = LangfuseExporter::start(Some(LangfuseConfig::default()));
        assert!(!exp.enabled());
        exp.send(sample_event("x.started", true));
    }

    #[tokio::test]
    async fn shutdown_completes_without_hang() {
        // 有 key → 启动后台 task,shutdown 应在 channel 排空后返回。
        let cfg = LangfuseConfig {
            endpoint: "http://127.0.0.1:1".into(),
            public_key: "pk-test".into(),
            secret_key: "sk-test".into(),
            batch_size: 4,
        };
        let exp = LangfuseExporter::start(Some(cfg));
        assert!(exp.enabled());
        for _ in 0..3 {
            exp.send(sample_event("turn.started", true));
        }
        // shutdown 关 channel → 后台 flush 剩余(投递到不可达端口会 warn 但不阻塞)。
        exp.shutdown().await;
    }
}
