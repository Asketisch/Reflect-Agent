//! headless `reflect exec` stdout 流的 JSONL 事件 writer。

use std::io::Write;

// ── JsonlWriter(JSONL 写入器)─────────────────────────────────────────────

pub struct JsonlWriter<W: Write> {
    inner: std::io::BufWriter<W>,
}

impl<W: Write> JsonlWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            inner: std::io::BufWriter::new(w),
        }
    }
    pub fn write_event(&mut self, event: &reflect_protocol::Event) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.inner, event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.inner.write_all(b"\n")?;
        self.inner.flush()?;
        Ok(())
    }
}
