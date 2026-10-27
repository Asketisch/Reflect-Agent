//! v1.x 会话统计聚合:按模型拆分的 token 用量 + 工具调用/压缩计数。
//!
//! 与 `list_sessions` 的轻量聚合(只读首行 + 单文件行扫描)不同,本模块
//! **全量扫描会话的全部文件**(活跃文件 + 轮转副本)。轮转是 rename 移动
//! 而非复制(见 `writer.rs::rotate`),每条记录只存在于一份文件中,故全扫
//! 既不遗漏也不重复 —— 长会话轮转后的 TokenCount 不会被漏计。
//!
//! 模型归属规则:`TokenCount.model` 存在时用完整 spec;旧 JSONL 无该字段
//! 时回退到**该文件**首行 `SessionMeta.model`(会话级默认模型),保证历史
//! 会话也有模型级视图。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use zhuye_protocol::{MemoryOp, MessageRole, ModelUsage, RolloutRecord, SessionStats, ThreadId};

/// 单会话完整统计(按模型拆分 + 工具调用/压缩次数 + 缓存分段)。
///
/// `session_id` 按**内部 id**(首行 SessionMeta)匹配,与 `find_session_path`
/// 一致;命中多份文件(活跃 + 轮转副本)时全部扫描。找不到任何文件 →
/// `None`。
pub fn session_stats(base: &Path, session_id: ThreadId) -> Option<SessionStats> {
    let files = find_all_session_files(base, session_id);
    if files.is_empty() {
        return None;
    }
    let mut acc = StatsAccumulator::default();
    for path in &files {
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        acc.scan_body(&body);
    }
    Some(acc.finish())
}

/// 跨会话按模型汇总:`<base>` 下所有 session 文件的 `TokenCount` 按模型
/// 分组累加,返回按 `total_tokens` 降序的用量列表。
///
/// 每条记录只存在于一份文件中(轮转移动语义),故全量 walk 一次即完整,
/// 无需按 session 去重。base 不存在 → 空列表。
pub fn all_model_usage(base: &Path) -> Vec<ModelUsage> {
    let mut acc = StatsAccumulator::default();
    for path in collect_jsonl_files(base) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        acc.scan_body(&body);
    }
    // 只取 per_model;会话级元数据对本函数无意义。
    acc.finish().per_model
}

/// 递归收集 `base` 下全部 `.jsonl` 文件路径(含日期子目录树)。
/// `_names/*.name` 等非 jsonl 文件天然被扩展名过滤排除。
fn collect_jsonl_files(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !base.exists() {
        return out;
    }
    fn scan(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    scan(base, &mut out);
    out
}

/// 按 `session_id` 收集该会话的**全部**文件(活跃 + 轮转副本)。
///
/// 双 ID 兼容:内部 `session_id`(首行 SessionMeta)或文件名(thread id)
/// 任一命中即收 —— 生产环境中两者一致(submission_loop 保证),但历史
/// 文件 / 外部构造的 recorder 可能出现分歧,与 `find_session_path`
/// (只匹配内部 id)相比这里放宽一档,避免统计漏文件。
/// 与 `super::find_file_by_session_id` 的区别:后者只返回活跃优先的
/// 单份文件,本函数返回全部 —— 统计需要完整记录集。
fn find_all_session_files(base: &Path, session_id: ThreadId) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !base.exists() {
        return out;
    }
    fn scan(dir: &Path, target: ThreadId, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, target, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                let by_internal = super::first_line_session_id(&path).as_ref() == Some(&target);
                // 文件名兜底:活跃文件 `<id>.jsonl` 的 stem 恰为 UUID 全串;
                // 轮转副本 `<id>.N.jsonl` 的 stem 含 `.N` 不匹配,但它们与
                // 活跃文件共享内部 SessionMeta,已由 by_internal 覆盖。
                let by_name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem == target.to_string())
                    .unwrap_or(false);
                if by_internal || by_name {
                    out.push(path);
                }
            }
        }
    }
    scan(base, session_id, &mut out);
    out
}

/// 统计 Assistant 消息 content 中的工具调用块数。
///
/// content 是 `Vec<ContentBlock>` 的 JSON 数组,`ToolUse` 序列化为
/// `{"type":"tool_use",...}`(见 `item.rs` 的 serde tag)。legacy 纯字符串
/// content(旧格式 User 消息)不是数组 → 0。
fn count_tool_uses(content: &serde_json::Value) -> u64 {
    let Some(arr) = content.as_array() else {
        return 0;
    };
    arr.iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .count() as u64
}

/// 单模型累计的中间态(带 cost 求和的存在性标记,finish 时才折成
/// `Option<f64>`,避免「无价调用被显示为 $0.00」)。
#[derive(Default)]
struct ModelAcc {
    provider: Option<String>,
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
    total_tokens: u64,
    cost_sum: f64,
    cost_present: bool,
}

impl ModelAcc {
    fn into_usage(self, model: String) -> ModelUsage {
        ModelUsage {
            model,
            provider: self.provider,
            calls: self.calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            cache_write_tokens: self.cache_write_tokens,
            total_tokens: self.total_tokens,
            cost_usd: self.cost_present.then_some(self.cost_sum),
        }
    }
}

/// 跨文件扫描的累计器。`scan_body` 逐份文件处理,文件内 `SessionMeta`
/// 的 model 作为该文件 `TokenCount`(无 model 字段时)的回退归属。
#[derive(Default)]
struct StatsAccumulator {
    session_id: Option<ThreadId>,
    model: Option<String>,
    started_at: Option<DateTime<Utc>>,
    /// 全部非空行数(与 `SessionInfo.message_count` 同口径的基数)。
    non_blank: usize,
    /// 遇到的 SessionMeta 行数(每份文件一份;message_count 要减掉)。
    meta_lines: usize,
    tool_calls: u64,
    compactions: u64,
    /// v1.x:各次压缩 before/after token 估算之和(压缩省量统计)。
    compaction_before_tokens: u64,
    compaction_after_tokens: u64,
    /// v1.x:记忆读 / 写次数(MemoryAccess 记录)。
    memory_reads: u64,
    memory_writes: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
    total_tokens: u64,
    cost_sum: f64,
    cost_present: bool,
    per_model: HashMap<String, ModelAcc>,
}

impl StatsAccumulator {
    fn scan_body(&mut self, body: &str) {
        // 文件级回退模型:本文件首行 SessionMeta 的 model。
        let mut file_meta_model: Option<String> = None;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.non_blank += 1;
            let Ok(rec) = serde_json::from_str::<RolloutRecord>(trimmed) else {
                continue;
            };
            match rec {
                RolloutRecord::SessionMeta {
                    session_id,
                    model,
                    started_at,
                    ..
                } => {
                    self.meta_lines += 1;
                    file_meta_model = Some(model.clone());
                    // 只取第一份文件的 meta 作会话级元数据(各文件 meta 相同)。
                    if self.session_id.is_none() {
                        self.session_id = Some(session_id);
                        self.model = Some(model);
                        self.started_at = Some(started_at);
                    }
                }
                RolloutRecord::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                } => {
                    self.tool_calls = self.tool_calls.saturating_add(count_tool_uses(&content));
                }
                RolloutRecord::Compaction {
                    before_tokens,
                    after_tokens,
                    ..
                } => {
                    self.compactions = self.compactions.saturating_add(1);
                    // v1.x:压缩前后 token 估算累计(旧 JSONL 无字段 → 0)。
                    self.compaction_before_tokens = self
                        .compaction_before_tokens
                        .saturating_add(before_tokens as u64);
                    self.compaction_after_tokens = self
                        .compaction_after_tokens
                        .saturating_add(after_tokens as u64);
                }
                RolloutRecord::MemoryAccess { op, .. } => match op {
                    MemoryOp::Read => {
                        self.memory_reads = self.memory_reads.saturating_add(1);
                    }
                    MemoryOp::Write => {
                        self.memory_writes = self.memory_writes.saturating_add(1);
                    }
                },
                RolloutRecord::TokenCount {
                    usage,
                    cost_usd,
                    model,
                    provider,
                    ..
                } => {
                    // 模型归属:record 自带 model → 旧记录回退文件级 meta model。
                    let key = model
                        .or_else(|| file_meta_model.clone())
                        .unwrap_or_default();
                    self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens as u64);
                    self.output_tokens = self
                        .output_tokens
                        .saturating_add(usage.output_tokens as u64);
                    self.cached_tokens = self
                        .cached_tokens
                        .saturating_add(usage.cached_tokens as u64);
                    self.cache_write_tokens = self
                        .cache_write_tokens
                        .saturating_add(usage.cache_write_tokens as u64);
                    self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens as u64);
                    if let Some(c) = cost_usd {
                        self.cost_sum += c;
                        self.cost_present = true;
                    }
                    let entry = self.per_model.entry(key.clone()).or_default();
                    entry.calls = entry.calls.saturating_add(1);
                    entry.input_tokens =
                        entry.input_tokens.saturating_add(usage.input_tokens as u64);
                    entry.output_tokens = entry
                        .output_tokens
                        .saturating_add(usage.output_tokens as u64);
                    entry.cached_tokens = entry
                        .cached_tokens
                        .saturating_add(usage.cached_tokens as u64);
                    entry.cache_write_tokens = entry
                        .cache_write_tokens
                        .saturating_add(usage.cache_write_tokens as u64);
                    entry.total_tokens =
                        entry.total_tokens.saturating_add(usage.total_tokens as u64);
                    if let Some(c) = cost_usd {
                        entry.cost_sum += c;
                        entry.cost_present = true;
                    }
                    // provider 取首次出现的值(同模型多 provider 路由时)。
                    if entry.provider.is_none() {
                        entry.provider = provider;
                    }
                }
                _ => {}
            }
        }
    }

    fn finish(self) -> SessionStats {
        let mut per_model: Vec<ModelUsage> = self
            .per_model
            .into_iter()
            .map(|(model, acc)| acc.into_usage(model))
            .collect();
        // 按 total_tokens 降序;并列时按模型名升序保证输出稳定。
        per_model.sort_by(|a, b| {
            b.total_tokens
                .cmp(&a.total_tokens)
                .then_with(|| a.model.cmp(&b.model))
        });
        SessionStats {
            // session_id 取扫描中首份 SessionMeta 的内部 id;无 meta 时
            // 用默认 id 兜底(仅 all_model_usage 的丢弃路径会走到)。
            session_id: self.session_id.unwrap_or_default(),
            model: self.model.unwrap_or_default(),
            started_at: self.started_at.unwrap_or_else(Utc::now),
            message_count: self.non_blank.saturating_sub(self.meta_lines),
            tool_calls: self.tool_calls,
            compactions: self.compactions,
            compaction_before_tokens: self.compaction_before_tokens,
            compaction_after_tokens: self.compaction_after_tokens,
            memory_reads: self.memory_reads,
            memory_writes: self.memory_writes,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            cache_write_tokens: self.cache_write_tokens,
            total_tokens: self.total_tokens,
            cost_usd: self.cost_present.then_some(self.cost_sum),
            per_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zhuye_protocol::{ContentBlock, TokenUsage, TurnId};

    /// 在 `base/YYYY/MM/DD/` 下写一份会话 JSONL,返回 tmp。
    /// `sid` 同时用作文件名与首行 SessionMeta 的 session_id(生产语义:
    /// submission_loop 保证两者一致)。
    fn write_session(sid: ThreadId, lines: &[String]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let dir = tmp
            .path()
            .join(now.format("%Y").to_string())
            .join(now.format("%m").to_string())
            .join(now.format("%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        tmp
    }

    fn meta_line(session_id: ThreadId, model: &str) -> String {
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id,
            model: model.to_string(),
            started_at: Utc::now(),
            workspace: None,
        })
        .unwrap()
    }

    fn token_line(
        usage: TokenUsage,
        cost: Option<f64>,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> String {
        serde_json::to_string(&RolloutRecord::TokenCount {
            turn_id: TurnId::new(),
            usage,
            cost_usd: cost,
            model: model.map(|s| s.to_string()),
            provider: provider.map(|s| s.to_string()),
            at: Utc::now(),
        })
        .unwrap()
    }

    fn assistant_with_tools(names: &[&str]) -> String {
        let blocks: Vec<ContentBlock> = names
            .iter()
            .enumerate()
            .map(|(i, n)| ContentBlock::ToolUse {
                id: format!("call-{i}"),
                name: (*n).to_string(),
                args: serde_json::json!({}),
            })
            .collect();
        serde_json::to_string(&RolloutRecord::Message {
            turn_id: TurnId::new(),
            role: MessageRole::Assistant,
            content: serde_json::to_value(&blocks).unwrap(),
        })
        .unwrap()
    }

    #[test]
    fn session_stats_groups_by_model_and_counts_tools() {
        let sid = ThreadId::new();
        let lines = vec![
            meta_line(sid, "openai/gpt-4o"),
            // 两次 gpt-4o 调用 + 一次 claude 调用。
            token_line(
                TokenUsage::new(100, 50, 0),
                Some(0.01),
                Some("openai/gpt-4o"),
                Some("openai"),
            ),
            token_line(
                TokenUsage::new(200, 80, 0),
                Some(0.02),
                Some("openai/gpt-4o"),
                Some("openai"),
            ),
            token_line(
                TokenUsage::new(300, 120, 50),
                Some(0.05),
                Some("anthropic/claude-3-5-sonnet-latest"),
                Some("anthropic"),
            ),
            // 两条 assistant 消息,共 3 次工具调用。
            assistant_with_tools(&["bash", "read"]),
            assistant_with_tools(&["edit"]),
        ];
        let tmp = write_session(sid, &lines);
        let stats = session_stats(tmp.path(), sid).expect("session should be found");

        assert_eq!(stats.session_id, sid);
        assert_eq!(stats.model, "openai/gpt-4o");
        // 6 行 - 1 行 meta = 5。
        assert_eq!(stats.message_count, 5);
        assert_eq!(stats.tool_calls, 3);
        assert_eq!(stats.compactions, 0);
        // 总量:input 600 / output 250 / total 850。
        assert_eq!(stats.input_tokens, 600);
        assert_eq!(stats.output_tokens, 250);
        assert_eq!(stats.total_tokens, 850);
        assert_eq!(stats.cached_tokens, 50);
        assert_eq!(stats.cost_usd, Some(0.08));

        // per_model 按 total_tokens 降序:claude(420) > gpt-4o(430)?
        // gpt-4o: 150+280=430,claude: 420 —— gpt-4o 在前。
        assert_eq!(stats.per_model.len(), 2);
        let gpt = &stats.per_model[0];
        assert_eq!(gpt.model, "openai/gpt-4o");
        assert_eq!(gpt.calls, 2);
        assert_eq!(gpt.input_tokens, 300);
        assert_eq!(gpt.output_tokens, 130);
        assert_eq!(gpt.total_tokens, 430);
        assert_eq!(gpt.provider.as_deref(), Some("openai"));
        assert_eq!(gpt.cost_usd, Some(0.03));
        let claude = &stats.per_model[1];
        assert_eq!(claude.model, "anthropic/claude-3-5-sonnet-latest");
        assert_eq!(claude.calls, 1);
        assert_eq!(claude.cached_tokens, 50);
        assert_eq!(claude.cost_usd, Some(0.05));
    }

    #[test]
    fn session_stats_falls_back_to_meta_model_for_legacy_records() {
        let sid = ThreadId::new();
        // 旧格式:TokenCount 无 model / provider 字段(手写 JSON,绕开
        // 结构体序列化以模拟历史文件)。
        let legacy = format!(
            r#"{{"type":"token_count","turn_id":"{}","usage":{{"input_tokens":10,"output_tokens":5,"cached_tokens":0,"cache_write_tokens":0,"total_tokens":15}},"cost_usd":0.001,"at":"2026-01-01T00:00:00Z"}}"#,
            TurnId::new()
        );
        let lines = vec![meta_line(sid, "openai/gpt-4o-mini"), legacy];
        let tmp = write_session(sid, &lines);
        let stats = session_stats(tmp.path(), sid).expect("session should be found");

        assert_eq!(stats.per_model.len(), 1);
        let m = &stats.per_model[0];
        // 回退到 SessionMeta.model。
        assert_eq!(m.model, "openai/gpt-4o-mini");
        assert_eq!(m.calls, 1);
        assert_eq!(m.total_tokens, 15);
        assert_eq!(m.provider, None);
    }

    #[test]
    fn session_stats_counts_compactions() {
        let sid = ThreadId::new();
        let lines = vec![
            meta_line(sid, "openai/gpt-4o"),
            serde_json::to_string(&RolloutRecord::Compaction {
                turn_id: TurnId::new(),
                strategy: "microcompact".into(),
                removed_count: 3,
                summary: String::new(),
                before_tokens: 12_000,
                after_tokens: 3_000,
            })
            .unwrap(),
        ];
        let tmp = write_session(sid, &lines);
        let stats = session_stats(tmp.path(), sid).unwrap();
        assert_eq!(stats.compactions, 1);
        // v1.x:压缩前后 token 累计 + 节省量派生。
        assert_eq!(stats.compaction_before_tokens, 12_000);
        assert_eq!(stats.compaction_after_tokens, 3_000);
        assert_eq!(stats.compaction_tokens_saved(), 9_000);
    }

    #[test]
    fn session_stats_counts_memory_access() {
        // v1.x:MemoryAccess 读/写记录分别计入 memory_reads / memory_writes。
        let sid = ThreadId::new();
        let mk = |op: &str| {
            format!(
                r#"{{"type":"memory_access","turn_id":"{}","op":"{op}","scope":"project","agent_type":"coder","chars":100,"entries":0,"at":"2026-10-21T00:00:00Z"}}"#,
                TurnId::new()
            )
        };
        let lines = vec![
            meta_line(sid, "openai/gpt-4o"),
            mk("read"),
            mk("read"),
            mk("write"),
        ];
        let tmp = write_session(sid, &lines);
        let stats = session_stats(tmp.path(), sid).unwrap();
        assert_eq!(stats.memory_reads, 2);
        assert_eq!(stats.memory_writes, 1);
    }

    #[test]
    fn session_stats_compaction_tokens_default_zero_for_legacy() {
        // 旧 JSONL 的 Compaction 无 before/after 字段 → 反序列化为 0,
        // 聚合不 panic、节省量为 0。
        let sid = ThreadId::new();
        let legacy = format!(
            r#"{{"type":"compaction","turn_id":"{}","strategy":"microcompact","removed_count":3,"summary":""}}"#,
            TurnId::new()
        );
        let lines = vec![meta_line(sid, "openai/gpt-4o"), legacy];
        let tmp = write_session(sid, &lines);
        let stats = session_stats(tmp.path(), sid).unwrap();
        assert_eq!(stats.compactions, 1);
        assert_eq!(stats.compaction_before_tokens, 0);
        assert_eq!(stats.compaction_after_tokens, 0);
        assert_eq!(stats.compaction_tokens_saved(), 0);
    }

    #[test]
    fn session_stats_returns_none_for_unknown_session() {
        let tmp = TempDir::new().unwrap();
        assert!(session_stats(tmp.path(), ThreadId::new()).is_none());
    }

    #[test]
    fn all_model_usage_aggregates_across_sessions() {
        // 同一 base 下两份会话文件,验证跨会话聚合。
        let sid_a = ThreadId::new();
        let lines_a = [
            meta_line(sid_a, "openai/gpt-4o"),
            token_line(
                TokenUsage::new(100, 50, 0),
                Some(0.01),
                Some("openai/gpt-4o"),
                Some("openai"),
            ),
        ];
        let sid_b = ThreadId::new();
        let lines_b = [
            meta_line(sid_b, "anthropic/claude"),
            token_line(
                TokenUsage::new(400, 200, 0),
                Some(0.09),
                Some("anthropic/claude"),
                Some("anthropic"),
            ),
        ];

        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let dir = tmp
            .path()
            .join(now.format("%Y").to_string())
            .join(now.format("%m").to_string())
            .join(now.format("%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{sid_a}.jsonl")),
            lines_a.join("\n") + "\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{sid_b}.jsonl")),
            lines_b.join("\n") + "\n",
        )
        .unwrap();

        let usage = all_model_usage(tmp.path());
        assert_eq!(usage.len(), 2);
        // 按 total_tokens 降序:claude(600) > gpt-4o(150)。
        assert_eq!(usage[0].model, "anthropic/claude");
        assert_eq!(usage[0].total_tokens, 600);
        assert_eq!(usage[0].cost_usd, Some(0.09));
        assert_eq!(usage[1].model, "openai/gpt-4o");
        assert_eq!(usage[1].total_tokens, 150);
        assert_eq!(usage[1].cost_usd, Some(0.01));
    }

    #[test]
    fn all_model_usage_empty_for_missing_base() {
        let tmp = TempDir::new().unwrap();
        assert!(all_model_usage(tmp.path()).is_empty());
    }

    #[test]
    fn session_stats_scans_rotated_siblings() {
        // 轮转是 rename 移动:TokenCount 落在 .1.jsonl,SessionMeta 在活跃
        // 文件。全扫两份文件才能拿到完整统计。
        let sid = ThreadId::new();
        let tmp = write_session(sid, &[meta_line(sid, "openai/gpt-4o")]);
        let now = Utc::now();
        let dir = tmp
            .path()
            .join(now.format("%Y").to_string())
            .join(now.format("%m").to_string())
            .join(now.format("%d").to_string());
        // 活跃文件只有 meta;轮转副本带 TokenCount。
        let rotated = dir.join(format!("{sid}.1.jsonl"));
        std::fs::write(
            &rotated,
            meta_line(sid, "openai/gpt-4o")
                + "\n"
                + &token_line(
                    TokenUsage::new(77, 33, 0),
                    None,
                    Some("openai/gpt-4o"),
                    Some("openai"),
                ),
        )
        .unwrap();

        let stats = session_stats(tmp.path(), sid).expect("session should be found");
        assert_eq!(stats.total_tokens, 110);
        assert_eq!(stats.per_model.len(), 1);
        assert_eq!(stats.per_model[0].calls, 1);
        // 两份文件各一行 meta → message_count = 3 行 - 2 meta = 1。
        assert_eq!(stats.message_count, 1);
    }
}
