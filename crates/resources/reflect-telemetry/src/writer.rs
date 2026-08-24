//! 两个 telemetry 存储的 JSONL writer。
//!
//! 模仿 `reflect-rollout::writer::JsonlRolloutWriter` 的轮转 pattern:
//! - [`SpanLogWriter`] 写 `<base>/log/reflect-YYYY-MM-DD.jsonl`,按天 +
//!   size 双维度轮转(`.1.jsonl` / `.2.jsonl` / `.3.jsonl`)。
//! - [`ModelIoWriter`] 写 `<base>/model-io/model-io-sess_<id>.jsonl`,
//!   仅按 size 轮转(同一 session 一个文件)。
//!
//! 两者都通过 [`crate::model::serialize_redacted`] 做 16 KiB 截断脱敏。
//! 写失败只 `tracing::warn!`,不中断在途 turn(best-effort,对齐 rollout 哲学)。

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use parking_lot::Mutex;

use crate::model::{
    MAX_ROTATED_FILES, MODEL_IO_SUBDIR, ModelIoRecord, ROTATE_AFTER_BYTES, SPAN_LOG_SUBDIR,
    TraceEvent, serialize_redacted,
};

/// 内部可变状态。
struct Inner {
    current_path: Option<PathBuf>,
    writer: Option<BufWriter<std::fs::File>>,
    current_size: u64,
}

impl Inner {
    fn new() -> Self {
        Self {
            current_path: None,
            writer: None,
            current_size: 0,
        }
    }
}

fn shift_rotations(parent: &Path, stem_no_ext: &str, ext: &str) -> std::io::Result<()> {
    // stem_no_ext = "reflect-2026-07-08" 或 "model-io-sess_xxx";ext = "jsonl"
    // 从高到低 shift:foo.3.jsonl 删,foo.2.jsonl→3,foo.1.jsonl→2,foo.jsonl→1
    for n in (1..=MAX_ROTATED_FILES).rev() {
        let src = if n == 1 {
            parent.join(format!("{stem_no_ext}.{ext}"))
        } else {
            parent.join(format!("{stem_no_ext}.{}.{ext}", n - 1))
        };
        let dst = parent.join(format!("{stem_no_ext}.{n}.{ext}"));
        if src.exists() {
            if dst.exists() {
                fs::remove_file(&dst)?;
            }
            fs::rename(&src, &dst)?;
        }
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// span 日志 writer(按天 + size 轮转)。
///
/// 文件名:`<base>/log/reflect-YYYY-MM-DD.jsonl`(UTC 日期)。
/// 当 `current_size >= ROTATE_AFTER_BYTES` 时轮转;日期变化也触发新文件
/// (跨天的 span 写到新日期文件)。
pub struct SpanLogWriter {
    base_dir: PathBuf,
    inner: Mutex<Inner>,
}

impl SpanLogWriter {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            inner: Mutex::new(Inner::new()),
        }
    }

    fn day_path(&self) -> PathBuf {
        let day = Utc::now().format("%Y-%m-%d");
        self.base_dir
            .join(SPAN_LOG_SUBDIR)
            .join(format!("reflect-{day}.jsonl"))
    }

    fn ensure_open(&self, g: &mut Inner) -> std::io::Result<()> {
        let path = self.day_path();
        // 日期变了 → 旧 writer 关闭,开新文件。
        if g.current_path.as_deref() != Some(path.as_path()) {
            if let Some(mut w) = g.writer.take() {
                let _ = w.flush();
            }
            ensure_parent(&path)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            g.writer = Some(BufWriter::new(file));
            g.current_path = Some(path);
            g.current_size = size;
        }
        Ok(())
    }

    /// 追加一条 span 事件。best-effort:失败只 warn。
    pub fn append(&self, ev: &TraceEvent) {
        let line = match serialize_redacted(ev) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "telemetry: serialize trace event failed");
                return;
            }
        };
        let line_bytes = line.len() as u64 + 1;
        let mut g = self.inner.lock();
        if let Err(e) = self.ensure_open(&mut g) {
            tracing::warn!(error = %e, "telemetry: open span log failed");
            return;
        }
        if let Some(w) = g.writer.as_mut()
            && let Err(e) = writeln!(w, "{line}").and_then(|()| w.flush())
        {
            tracing::warn!(error = %e, "telemetry: write span log failed");
            return;
        }
        g.current_size = g.current_size.saturating_add(line_bytes);
        if g.current_size >= ROTATE_AFTER_BYTES {
            self.rotate(&mut g);
        }
    }

    fn rotate(&self, g: &mut Inner) {
        let Some(current) = g.current_path.clone() else {
            return;
        };
        if let Some(mut w) = g.writer.take() {
            let _ = w.flush();
        }
        let Some(parent) = current.parent() else {
            return;
        };
        let Some(stem) = current
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".jsonl").map(String::from))
        else {
            return;
        };
        if let Err(e) = shift_rotations(parent, &stem, "jsonl") {
            tracing::warn!(error = %e, "telemetry: rotate span log shift failed");
        }
        // 重开新文件。
        match OpenOptions::new().create(true).append(true).open(&current) {
            Ok(file) => {
                g.writer = Some(BufWriter::new(file));
                g.current_size = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "telemetry: reopen span log failed");
            }
        }
    }
}

/// model-io writer(按 session 一个文件,size 轮转)。
///
/// 文件名:`<base>/model-io/model-io-sess_<session_id>.jsonl`。
pub struct ModelIoWriter {
    base_dir: PathBuf,
    session_id: String,
    inner: Mutex<Inner>,
}

impl ModelIoWriter {
    pub fn new(base_dir: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            base_dir: base_dir.into(),
            session_id: session_id.into(),
            inner: Mutex::new(Inner::new()),
        }
    }

    fn session_path(&self) -> PathBuf {
        self.base_dir
            .join(MODEL_IO_SUBDIR)
            .join(format!("model-io-sess_{}.jsonl", self.session_id))
    }

    fn ensure_open(&self, g: &mut Inner) -> std::io::Result<()> {
        if g.current_path.is_some() {
            return Ok(());
        }
        let path = self.session_path();
        ensure_parent(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        g.writer = Some(BufWriter::new(file));
        g.current_path = Some(path);
        g.current_size = size;
        Ok(())
    }

    /// 追加一条 model-io 记录。best-effort:失败只 warn。
    pub fn append(&self, rec: &ModelIoRecord) {
        let line = match serialize_redacted(rec) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "telemetry: serialize model-io failed");
                return;
            }
        };
        let line_bytes = line.len() as u64 + 1;
        let mut g = self.inner.lock();
        if let Err(e) = self.ensure_open(&mut g) {
            tracing::warn!(error = %e, "telemetry: open model-io failed");
            return;
        }
        if let Some(w) = g.writer.as_mut()
            && let Err(e) = writeln!(w, "{line}").and_then(|()| w.flush())
        {
            tracing::warn!(error = %e, "telemetry: write model-io failed");
            return;
        }
        g.current_size = g.current_size.saturating_add(line_bytes);
        if g.current_size >= ROTATE_AFTER_BYTES {
            self.rotate(&mut g);
        }
    }

    fn rotate(&self, g: &mut Inner) {
        let Some(current) = g.current_path.clone() else {
            return;
        };
        if let Some(mut w) = g.writer.take() {
            let _ = w.flush();
        }
        let Some(parent) = current.parent() else {
            return;
        };
        let Some(stem) = current
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".jsonl").map(String::from))
        else {
            return;
        };
        if let Err(e) = shift_rotations(parent, &stem, "jsonl") {
            tracing::warn!(error = %e, "telemetry: rotate model-io shift failed");
        }
        match OpenOptions::new().create(true).append(true).open(&current) {
            Ok(file) => {
                g.writer = Some(BufWriter::new(file));
                g.current_size = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "telemetry: reopen model-io failed");
            }
        }
    }
}

/// 读最近 N 天的 span 日志文件路径(供 TUI `/traces` overlay 读取)。
///
/// 返回按日期降序的 `(date_str, path)` 列表。只返回存在的文件。
pub fn list_span_log_files(base_dir: &Path, days: usize) -> Vec<(String, PathBuf)> {
    let log_dir = base_dir.join(SPAN_LOG_SUBDIR);
    let mut out = Vec::new();
    let now = Utc::now();
    for i in 0..days.max(1) {
        let day = now - chrono::Duration::days(i as i64);
        let day_str = day.format("%Y-%m-%d").to_string();
        let path = log_dir.join(format!("reflect-{day_str}.jsonl"));
        if path.exists() {
            out.push((day_str, path));
        }
    }
    out
}

/// 列出 `model-io/` 子目录下所有 session 的 `(session_id, path, mtime)`。
///
/// 与 [`list_span_log_files`] 对称 —— 让读写 API 完整,CLI `/traces` 与
/// TUI overlay 都能复用。扫描 `model-io-sess_<id>.jsonl`(跳过 `.N.jsonl`
/// 轮转副本与 `.DS_Store` 等噪声),按文件 `mtime` 降序(最近优先)。
///
/// `session_id` 从文件名解析(去掉 `model-io-sess_` 前缀与 `.jsonl` 后缀)。
pub fn list_model_io_files(base_dir: &Path) -> Vec<(String, PathBuf, std::time::SystemTime)> {
    let dir = base_dir.join(MODEL_IO_SUBDIR);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // 只取 active 文件(model-io-sess_<id>.jsonl),跳过 .N 轮转副本与噪声。
        let Some(id) = name
            .strip_prefix("model-io-sess_")
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let path = entry.path();
        // 读 mtime 用于排序(失败时退化为 EPOCH,排到最后)。
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push((id.to_string(), path, mtime));
    }
    // 按 mtime 降序(最近修改的 session 排最前)。
    out.sort_by_key(|t| std::cmp::Reverse(t.2));
    out
}

/// 读一个 model-io session 文件的全部记录(逐行解析,跳过坏行)。
pub fn read_model_io_file(path: &Path) -> Vec<ModelIoRecord> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<ModelIoRecord>(line) {
            out.push(rec);
        }
    }
    out
}

/// 读一个 span 日志文件的全部事件(逐行解析,跳过坏行)。
pub fn read_span_log_file(path: &Path) -> Vec<TraceEvent> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<TraceEvent>(line) {
            out.push(ev);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TraceEvent, new_span_id, new_trace_id};
    use tempfile::tempdir;

    fn make_event(trace_id: &str, span_id: &str) -> TraceEvent {
        TraceEvent {
            timestamp: Utc::now(),
            level: crate::model::Level::Info,
            event: "test.event".into(),
            module: "test".into(),
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            session_id: Some("s1".into()),
            turn_id: None,
            tool_call_id: None,
            status: None,
            duration_ms: None,
            context: None,
        }
    }

    #[test]
    fn span_log_writer_appends_one_line() {
        let dir = tempdir().unwrap();
        let w = SpanLogWriter::new(dir.path());
        let ev = make_event("t1", &new_span_id());
        w.append(&ev);
        let files = list_span_log_files(dir.path(), 1);
        assert_eq!(files.len(), 1);
        let events = read_span_log_file(&files[0].1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "test.event");
    }

    #[test]
    fn span_log_writer_rotates_on_size() {
        let dir = tempdir().unwrap();
        let w = SpanLogWriter::new(dir.path());
        // 每条事件 context 塞 4 KiB,写 200 条 → ~800 KiB → 触发多次轮转。
        let trace = new_trace_id();
        let span = new_span_id();
        for _ in 0..200 {
            let mut ev = make_event(&trace, &span);
            ev.context = Some(serde_json::json!({"blob": "x".repeat(4096)}));
            w.append(&ev);
        }
        let files = list_span_log_files(dir.path(), 1);
        let log_dir = dir.path().join(SPAN_LOG_SUBDIR);
        // 应存在至少一个 .1.jsonl 轮转副本。
        let has_rotated = std::fs::read_dir(&log_dir)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(".1.jsonl"))
            })
            .unwrap_or(false);
        assert!(has_rotated, "expected a rotated .1.jsonl");
        let _ = files;
    }

    #[test]
    fn model_io_writer_appends_one_record() {
        let dir = tempdir().unwrap();
        let w = ModelIoWriter::new(dir.path(), "sess-1");
        let rec = ModelIoRecord {
            started_at: Utc::now(),
            completed_at: Utc::now(),
            duration_ms: 100,
            attempt: 1,
            request_id: "r1".into(),
            trace_id: "t1".into(),
            turn_id: None,
            session_id: "sess-1".into(),
            query_source: "main_turn".into(),
            model: crate::model::ModelRef::default(),
            request: serde_json::json!({}),
            response: serde_json::json!({}),
            usage: crate::model::UsageSnapshot::default(),
        };
        w.append(&rec);
        let path = dir
            .path()
            .join(MODEL_IO_SUBDIR)
            .join("model-io-sess_sess-1.jsonl");
        let recs = read_model_io_file(&path);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].request_id, "r1");
    }

    #[test]
    fn span_log_redacts_large_context() {
        let dir = tempdir().unwrap();
        let w = SpanLogWriter::new(dir.path());
        let mut ev = make_event("t1", &new_span_id());
        ev.context = Some(serde_json::json!({"big": "y".repeat(20_000)}));
        w.append(&ev);
        let files = list_span_log_files(dir.path(), 1);
        let content = std::fs::read_to_string(&files[0].1).unwrap();
        assert!(content.contains(crate::model::REDACTION_MARKER));
        assert!(content.len() < 20_000);
    }

    #[test]
    fn read_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            "not json\n{}\n{\"event\":\"x\",\"timestamp\":\"2026-07-08T00:00:00Z\",\"level\":\"info\",\"module\":\"m\",\"trace_id\":\"t\",\"span_id\":\"s\"}\n",
        )
        .unwrap();
        let events = read_span_log_file(&path);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "x");
    }
}
