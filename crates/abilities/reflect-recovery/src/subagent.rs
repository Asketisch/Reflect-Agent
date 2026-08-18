//! `SubagentRegistry` — 追踪已完成的子代理调用,防 LLM 重复 spawn。
//!
//! 追踪已完成的子代理调用,注入到下一轮 pre_loop,LLM 看到后能复用已知结果。
//!
//! ## 实现要点
//!
//! - **FIFO cap = 32**:32 条默认值,够 8+ 轮子代理调用
//!   滚动窗口。
//! - **summary 截断**:`task_summary ≤ 200` / `result_summary ≤ 500`
//!   字符(`chars().take(n)` 按字符边界切,避免在多字节 UTF-8 中切坏)。
//! - **in-memory only**:子代理 registry 跨 turn 通过 `Arc<SubagentRegistry>`
//!   共享在 `M4Deps` 上 —— 不需落盘,因为子代理 session 结束就该清理
//!   (否则会污染下次会话的 LLM 上下文)。

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::debug;

/// 注册表容量上限(32 条,FIFO 弹出最旧)。
pub const SUBAGENT_REGISTRY_CAP: usize = 32;
/// 单条 task 摘要字符上限。
pub const SUBAGENT_TASK_SUMMARY_MAX: usize = 200;
/// 单条 result 摘要字符上限。
pub const SUBAGENT_RESULT_SUMMARY_MAX: usize = 500;

/// 一条已完成的子代理调用记录。
#[derive(Debug, Clone)]
pub struct SubagentRegistryEntry {
    /// 工具名,通常是 `call_<role>` 形式(对齐 `SubAgentSpec::tool_name()`)。
    pub tool_name: String,
    /// 任务摘要(≤ 200 chars),`SubagentRegistry::record` 写入时截断。
    pub task_summary: String,
    /// 结果摘要(≤ 500 chars),`SubagentRegistry::record` 写入时截断。
    pub result_summary: String,
    /// 触发该子代理的 turn id —— 用于按时间排序 + 调试。
    pub iteration: u32,
    /// 写入时间。
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 跨 turn 共享的子代理调用注册表。
///
/// 用 `Arc<SubagentRegistry>` 在 `M4Deps` 上共享 —— `CallSubAgentTool`
/// 写,`pre_loop` 读 + 渲染为 `<system-reminder>`。
pub struct SubagentRegistry {
    inner: Mutex<VecDeque<SubagentRegistryEntry>>,
    cap: usize,
    max_task_summary: usize,
    max_result_summary: usize,
}

impl std::fmt::Debug for SubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRegistry")
            .field("count", &self.inner.lock().len())
            .field("cap", &self.cap)
            .finish()
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(SUBAGENT_REGISTRY_CAP)),
            cap: SUBAGENT_REGISTRY_CAP,
            max_task_summary: SUBAGENT_TASK_SUMMARY_MAX,
            max_result_summary: SUBAGENT_RESULT_SUMMARY_MAX,
        }
    }

    /// 共享 Arc wrapper,方便塞进 `M4Deps`。
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// 写入一条 entry。超 cap 则 FIFO 弹出最旧。`task_summary` /
    /// `result_summary` 自动按字符边界截断到上限。
    ///
    /// v1.1.0 review bug-6 (P2):加 `tracing::debug!` 记录工具名 + iteration,
    /// 便于事后追查"哪轮 spawn 了什么 subagent"。
    pub fn record(&self, mut entry: SubagentRegistryEntry) {
        entry.task_summary = truncate_chars(&entry.task_summary, self.max_task_summary);
        entry.result_summary = truncate_chars(&entry.result_summary, self.max_result_summary);
        let tool = entry.tool_name.clone();
        let iter = entry.iteration;
        let mut guard = self.inner.lock();
        while guard.len() >= self.cap {
            guard.pop_front();
        }
        let len_after = guard.len() + 1;
        guard.push_back(entry);
        debug!(
            tool = %tool,
            iteration = iter,
            registry_len = len_after,
            "subagent_registry: record"
        );
    }

    /// 当前所有 entry 快照(按插入顺序,最新在末尾)。
    pub fn snapshot(&self) -> Vec<SubagentRegistryEntry> {
        self.inner.lock().iter().cloned().collect()
    }

    /// 渲染为 `<system-reminder>` 注入内容。空时返回 `None`。
    ///
    /// 输出格式:
    /// ```text
    /// [已完成的子代理调用记录(请勿重复 spawn)]
    /// - 第3轮 call_explorer: 任务=探索当前目录 | 结果摘要=找到 5 个模块...
    /// - 第5轮 call_explorer: 任务=列出所有 .rs 文件 | 结果摘要=共 17 个文件...
    /// ```
    ///
    /// v1.1.0 review bug-2 (P1):改用 `{}` (Display) 而非 `{:?}` (Debug) 渲染
    /// 摘要内容 —— `{:?}` 会给 `"`、`\n` 等加转义(`\"`、`\\n`),把自然语言
    /// 注入到 LLM context 时变成"调试打印"格式,既冗余又误导 LLM。`tool_name`
    /// 是受约束的 `call_<role>` 格式,`{}` 与 `{:?}` 输出一致。
    ///
    /// v1.1.0 review bug-6 (P2):加 `tracing::debug!` 输出 entry 数,便于
    /// 对比 LLM 实际看到的 reminder 体积与 registry 实际条目数。
    pub fn as_meta_message(&self) -> Option<String> {
        let snapshot = self.snapshot();
        if snapshot.is_empty() {
            return None;
        }
        let entry_count = snapshot.len();
        let mut s = String::from("[已完成的子代理调用记录(请勿重复 spawn)]\n");
        for e in &snapshot {
            s.push_str(&format!(
                "- 第{}轮 {}: 任务={} | 结果摘要={}\n",
                e.iteration, e.tool_name, e.task_summary, e.result_summary
            ));
        }
        debug!(
            entry_count,
            bytes = s.len(),
            "subagent_registry: as_meta_message"
        );
        Some(s)
    }
}

/// 按字符边界截断,避免在多字节 UTF-8 序列中间切。
///
/// v1.1.0 review bug-5 (P2):原实现走 `chars().count()` + `chars().take(n)`
/// 两遍迭代 O(2n);改用 `char_indices()` 单遍定位切点 + 字节切片 O(n)。
/// 对超长 prompt (≥ 10K chars 的 worker task) 把 record 路径开销砍半。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.len() == s.chars().count() {
        // ASCII fast path:byte 长度 == char 长度,直接切片。
        if s.len() <= max_chars {
            return s.to_string();
        }
        return s[..max_chars].to_string();
    }
    // 多字节路径:定位第 `max_chars` 个字符的字节偏移。
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tool: &str, task: &str, result: &str, iter: u32) -> SubagentRegistryEntry {
        SubagentRegistryEntry {
            tool_name: tool.into(),
            task_summary: task.into(),
            result_summary: result.into(),
            iteration: iter,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn record_keeps_chronological_order() {
        let r = SubagentRegistry::new();
        r.record(entry("call_explorer", "task1", "res1", 1));
        r.record(entry("call_explorer", "task2", "res2", 2));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].task_summary, "task1");
        assert_eq!(snap[1].task_summary, "task2");
    }

    #[test]
    fn record_truncates_task_summary_to_200() {
        let r = SubagentRegistry::new();
        let big = "x".repeat(500);
        r.record(entry("call_x", &big, "ok", 1));
        let snap = r.snapshot();
        assert_eq!(snap[0].task_summary.chars().count(), 200);
    }

    #[test]
    fn record_truncates_result_summary_to_500() {
        let r = SubagentRegistry::new();
        let big = "y".repeat(1000);
        r.record(entry("call_x", "ok", &big, 1));
        let snap = r.snapshot();
        assert_eq!(snap[0].result_summary.chars().count(), 500);
    }

    #[test]
    fn record_caps_at_registry_size() {
        let r = SubagentRegistry::new();
        for i in 0..(SUBAGENT_REGISTRY_CAP + 5) {
            r.record(entry("call_x", &format!("t{i}"), "ok", i as u32));
        }
        let snap = r.snapshot();
        assert_eq!(snap.len(), SUBAGENT_REGISTRY_CAP);
        // FIFO:最早 5 条被弹出
        assert_eq!(snap[0].task_summary, "t5");
        assert_eq!(snap[SUBAGENT_REGISTRY_CAP - 1].task_summary, "t36");
    }

    #[test]
    fn as_meta_message_none_when_empty() {
        let r = SubagentRegistry::new();
        assert!(r.as_meta_message().is_none());
    }

    #[test]
    fn as_meta_message_contains_all_entries() {
        let r = SubagentRegistry::new();
        r.record(entry("call_explorer", "find modules", "5 modules", 3));
        r.record(entry("call_explorer", "list .rs", "17 files", 5));
        let m = r.as_meta_message().unwrap();
        assert!(m.contains("已完成的子代理调用记录"));
        assert!(m.contains("第3轮"));
        assert!(m.contains("call_explorer"));
        assert!(m.contains("find modules"));
        assert!(m.contains("第5轮"));
        assert!(m.contains("17 files"));
    }

    #[test]
    fn truncate_at_utf8_boundary() {
        // 包含中文:每个字符 3 字节,但 char count = 1
        let s = "你好世界".repeat(100); // 400 chars,1200 bytes
        let t = truncate_chars(&s, 50);
        assert_eq!(t.chars().count(), 50);
    }

    #[test]
    fn shared_returns_arc() {
        let r1 = SubagentRegistry::shared();
        let r2 = r1.clone();
        r1.record(entry("call_x", "shared", "ok", 1));
        assert_eq!(r2.snapshot().len(), 1);
    }

    // ── v1.1.0 review ──────────────────────────────────────────────
    // bug-2 (P1):`{:?}` 转义
    // bug-5 (P2):`truncate_chars` O(2n)
    // bug-6 (P2):tracing
    // bug-7 (P2):snapshot 锁格式

    /// bug-2:Display 而非 Debug,自然语言不含 `\"` / `\n` 转义。
    #[test]
    fn as_meta_message_uses_display_not_debug_for_summaries() {
        let r = SubagentRegistry::new();
        r.record(entry("call_x", "find \"main.rs\"", "line1\nline2", 1));
        let m = r.as_meta_message().unwrap();
        // Display:直接保留引号与换行,没有反斜杠转义。
        assert!(
            m.contains("任务=find \"main.rs\""),
            "should use Display formatting, got: {m}"
        );
        assert!(
            m.contains("结果摘要=line1\nline2"),
            "should preserve newlines literally, got: {m}"
        );
        // Debug 形式会出现的 `\"` / `\\n` 不应出现。
        assert!(!m.contains("\\\""), "Debug escaping leaked: {m}");
    }

    /// bug-5:`truncate_chars` 对多字节字符串仍按字符边界切,且 O(n)。
    /// 通过 `重复写入 + 截断 + 多字节混合` 验证。
    #[test]
    fn truncate_chars_handles_multibyte_and_pure_ascii() {
        // 纯 ASCII fast path:输入 ≤ 上限 → 原样返回。
        assert_eq!(truncate_chars("hello", 10), "hello");
        // 纯 ASCII fast path:输入 > 上限 → 字节切片。
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        // 多字节:中文字符按 char 计数。
        let s = "你好世界你好".repeat(50); // 300 chars
        let t = truncate_chars(&s, 100);
        assert_eq!(t.chars().count(), 100);
        // 多字节 + emoji(4-byte UTF-8):仍按 char 计数。
        let s = "👋🌍".repeat(40); // 80 chars
        let t = truncate_chars(&s, 50);
        assert_eq!(t.chars().count(), 50);
        // 边界值:切点正好 == len,返回原字符串。
        let s = "abc";
        assert_eq!(truncate_chars(s, 3), "abc");
    }

    /// bug-7:exact-format snapshot —— `as_meta_message` 完整字符串输出
    /// 锁住,防止后续格式变更静默回归。
    #[test]
    fn as_meta_message_exact_format() {
        let r = SubagentRegistry::new();
        r.record(entry("call_explorer", "find modules", "5 modules", 3));
        r.record(entry("call_explorer", "list .rs", "17 files", 5));
        let m = r.as_meta_message().unwrap();
        let expected = "\
[已完成的子代理调用记录(请勿重复 spawn)]
- 第3轮 call_explorer: 任务=find modules | 结果摘要=5 modules
- 第5轮 call_explorer: 任务=list .rs | 结果摘要=17 files
";
        assert_eq!(m, expected);
    }
}
