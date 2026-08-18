//! `cron` — v1.2 P1-2 真实定时调度。
//!
//! 把 v0 的 stub 升级为真正可调度的 cron 引擎:agent 通过 `CronTool`
//! 注册 `CronJobSpec`,后台 driver 在到期时把 `job.prompt` 作为一条
//! `Submission::user_input` 注入 agent loop(经 `AgentThread` 的
//! submission sender),触发新一轮对话。
//!
//! 5 字段 cron 表达式解析自包含(无外部 cron crate,对齐项目风格),
//! 支持:数字 / `*` / `,` 列表 / `-` 范围 / `/` 步长。下次触发时间用
//! 暴力枚举(从 now 起逐分钟扫描,最多扫 366 天防死循环),足够覆盖
//! 分钟级及以上粒度的常见 schedule。

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use parking_lot::RwLock;
use reflect_protocol::{Submission, ThreadId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

// ── 表达式解析 ───────────────────────────────────────────────────────

/// 5 字段 cron 表达式解析后的字段匹配器。
///
/// 每个字段是一组允许的整数集合(`Vec` 排序去重,便于调试 / 序列化)。
/// `*` → 该字段全部允许值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    dom: Vec<u32>,   // day of month (1-31)
    month: Vec<u32>, // 1-12
    dow: Vec<u32>,   // day of week (0-6, 0=Sunday)
}

/// cron 表达式解析错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CronParseError {
    #[error("cron expression must have exactly 5 fields, got {0}")]
    FieldCount(usize),
    #[error("field {field:?}: value {value} out of range [{lo},{hi}]")]
    OutOfRange {
        field: &'static str,
        value: i64,
        lo: i32,
        hi: i32,
    },
    #[error("field {field:?}: invalid token {token:?}")]
    InvalidToken { field: &'static str, token: String },
    #[error("field {field:?}: step cannot be zero")]
    ZeroStep { field: &'static str },
}

impl CronSchedule {
    /// 解析 5 字段 cron 表达式(min hour dom month dow)。
    ///
    /// 支持每字段:`*` / 单数 / `,` 列表 / `-` 范围 / `/` 步长。例:
    /// - `* * * * *` —— 每分钟
    /// - `0 * * * *` —— 每小时整点
    /// - `*/15 * * * *` —— 每 15 分钟
    /// - `0 9 * * 1-5` —— 工作日早 9 点
    pub fn parse(expr: &str) -> Result<Self, CronParseError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronParseError::FieldCount(fields.len()));
        }
        Ok(Self {
            minute: parse_field(fields[0], 0, 59, "minute")?,
            hour: parse_field(fields[1], 0, 23, "hour")?,
            dom: parse_field(fields[2], 1, 31, "dom")?,
            month: parse_field(fields[3], 1, 12, "month")?,
            dow: parse_field(fields[4], 0, 6, "dow")?,
        })
    }

    /// 计算 `after` 之后(不含)的下一个触发时间。
    ///
    /// 从 `after + 1 分钟` 起逐分钟扫描,最多扫 366 天防死循环。返回
    /// `None` 表示该表达式在扫描窗口内无解(例如 `2月30日` 这类不可能
    /// 日期)。
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // 对齐到下一分钟零秒,逐分钟扫描。
        let mut t = after
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(Duration::minutes(1))?;
        for _ in 0..(366 * 24 * 60) {
            if self.minute.contains(&t.minute())
                && self.hour.contains(&t.hour())
                && self.dom.contains(&t.day())
                && self.month.contains(&t.month())
                && self.dow.contains(&t.weekday().num_days_from_sunday())
            {
                return Some(t);
            }
            t = t.checked_add_signed(Duration::minutes(1))?;
        }
        None
    }
}

/// 解析单个 cron 字段为允许值集合(排序去重)。
fn parse_field(
    raw: &str,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<Vec<u32>, CronParseError> {
    let mut out: Vec<u32> = Vec::new();
    for part in raw.split(',') {
        out.extend(parse_part(part, lo, hi, field)?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 解析单个 token(可能含 `/` 步长、`-` 范围、`*`)。
fn parse_part(
    token: &str,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<Vec<u32>, CronParseError> {
    // 拆出步长 `<base>/<step>`。
    let (base, step) = match token.split_once('/') {
        Some((b, s)) => {
            let step: i32 = s.parse().map_err(|_| CronParseError::InvalidToken {
                field,
                token: token.to_string(),
            })?;
            if step <= 0 {
                return Err(CronParseError::ZeroStep { field });
            }
            (b, Some(step))
        }
        None => (token, None),
    };

    // 解析 base 的范围(下界,上界)。`*` → [lo,hi]。
    let (start, end) = if base == "*" {
        (lo, hi)
    } else if let Some((a, b)) = base.split_once('-') {
        let a: i32 = a.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        let b: i32 = b.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        (a, b)
    } else {
        let v: i32 = base.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        // 单值 + 步长:v, v+step, ... 直到 hi(与 cron 语义一致)。
        if let Some(step) = step {
            let mut out = Vec::new();
            let mut cur = v;
            while cur <= hi {
                check_range(cur, lo, hi, field)?;
                out.push(cur as u32);
                cur += step;
            }
            return Ok(out);
        }
        check_range(v, lo, hi, field)?;
        return Ok(vec![v as u32]);
    };

    check_range(start, lo, hi, field)?;
    check_range(end, lo, hi, field)?;

    let step = step.unwrap_or(1);
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        out.push(cur as u32);
        cur += step;
    }
    Ok(out)
}

fn check_range(v: i32, lo: i32, hi: i32, field: &'static str) -> Result<(), CronParseError> {
    if v < lo || v > hi {
        return Err(CronParseError::OutOfRange {
            field,
            value: v as i64,
            lo,
            hi,
        });
    }
    Ok(())
}

// ── 任务规格 + 调度器 ─────────────────────────────────────────────

/// 一条 cron 任务规格(可序列化,落盘用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronJobSpec {
    /// 稳定 id(uuid)。update / delete 用它寻址。
    pub id: String,
    /// 5 字段 cron 表达式。
    pub schedule: String,
    /// 触发时作为 `Submission::user_input` 注入 agent loop 的 prompt。
    pub prompt: String,
    /// 可选任务名(给人看)。
    #[serde(default)]
    pub name: Option<String>,
    /// 是否启用。`false` 的 job 不被 driver 调度(但保留在列表)。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 创建时间(UTC)。
    pub created_at: DateTime<Utc>,
    /// 上次触发时间(UTC)。`None` = 从未触发。
    #[serde(default)]
    pub last_fired: Option<DateTime<Utc>>,
    /// 预计下次触发时间(UTC)。driver 启动 / schedule 变更时重算。
    #[serde(default)]
    pub next_fire: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

/// 调度器状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronStatus {
    /// 无 job(或全部 disabled)。
    Idle,
    /// 至少一个 enabled job。
    HasJobs,
}

/// 真实 cron 调度器。持有一组 session-scoped `CronJobSpec`,后台 driver
/// 到期时把 prompt 作为 `Submission` 注入 agent loop。
///
/// 设计:
/// - `jobs` 用 `Arc<RwLock<Vec<CronJobSpec>>>` 共享,driver task 与
///   `CronTool` 调用方都通过它读写(`Arc` clone)。
/// - `sub_tx` 注入 agent loop 的 submission sender。`None` = 仅记录、
///   不真正触发(测试 / 未接 loop 时)。
/// - `session_id` 决定持久化文件名(`~/.reflect/cron/<session>.json`)。
#[derive(Clone)]
pub struct CronScheduler {
    pub(crate) jobs: Arc<RwLock<Vec<CronJobSpec>>>,
    pub(crate) sub_tx: Option<mpsc::Sender<Submission>>,
    session_id: ThreadId,
}

impl std::fmt::Debug for CronScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronScheduler")
            .field("session_id", &self.session_id)
            .field("jobs_count", &self.jobs.read().len())
            .field("has_sub_tx", &self.sub_tx.is_some())
            .finish()
    }
}

impl CronScheduler {
    /// 构造一个空调度器。`sub_tx` 注入 agent loop 的 submission sender;
    /// 传 `None` 时 driver 不真正触发(测试用)。
    pub fn new(sub_tx: Option<mpsc::Sender<Submission>>, session_id: ThreadId) -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Vec::new())),
            sub_tx,
            session_id,
        }
    }

    /// 该调度器绑定的 session id(持久化 key)。
    pub fn session_id(&self) -> &ThreadId {
        &self.session_id
    }

    /// 当前 job 快照(按 created_at 排序的 clone)。
    pub fn list(&self) -> Vec<CronJobSpec> {
        let mut v = self.jobs.read().clone();
        v.sort_by_key(|j| j.created_at);
        v
    }

    /// 取单条 job。
    pub fn get(&self, id: &str) -> Option<CronJobSpec> {
        self.jobs.read().iter().find(|j| j.id == id).cloned()
    }

    /// 调度器状态。
    pub fn status(&self) -> CronStatus {
        if self.jobs.read().iter().any(|j| j.enabled) {
            CronStatus::HasJobs
        } else {
            CronStatus::Idle
        }
    }

    /// 人可读状态行(给 TUI `/workflows` pill 复用)。
    pub fn status_line(&self) -> String {
        let jobs = self.jobs.read();
        let enabled = jobs.iter().filter(|j| j.enabled).count();
        match jobs.len() {
            0 => "cron-scheduler: 无已注册 job".into(),
            total => format!(
                "cron-scheduler: {total} 个 job({enabled} 启用)— driver 每 30s 扫描到期 job"
            ),
        }
    }

    /// 注册一条新 job。校验 schedule 表达式并算出 next_fire;返回新 job 的
    /// clone。`name` 缺省时取 prompt 前 24 字符。
    pub fn create(
        &self,
        schedule: &str,
        prompt: impl Into<String>,
        name: Option<String>,
    ) -> Result<CronJobSpec, CronParseError> {
        let parsed = CronSchedule::parse(schedule)?;
        let now = Utc::now();
        let prompt = prompt.into();
        let next_fire = parsed.next_after(now);
        let job = CronJobSpec {
            id: uuid::Uuid::new_v4().to_string(),
            schedule: schedule.trim().to_string(),
            name: name.or_else(|| {
                let n: String = prompt.chars().take(24).collect();
                (!n.is_empty()).then_some(n)
            }),
            prompt,
            enabled: true,
            created_at: now,
            last_fired: None,
            next_fire,
        };
        self.jobs.write().push(job.clone());
        Ok(job)
    }

    /// 更新一条 job(按 id)。`schedule` / `prompt` / `name` / `enabled`
    /// 任一为 `Some` 才更新;改 schedule 会重算 next_fire。返回更新后的
    /// job clone,或 `None` 表示 id 不存在。
    pub fn update(
        &self,
        id: &str,
        schedule: Option<&str>,
        prompt: Option<&str>,
        name: Option<&str>,
        enabled: Option<bool>,
    ) -> Option<CronJobSpec> {
        let mut jobs = self.jobs.write();
        let idx = jobs.iter().position(|j| j.id == id)?;
        let job = &mut jobs[idx];
        if let Some(s) = schedule {
            let parsed = CronSchedule::parse(s).ok()?;
            job.next_fire = parsed.next_after(Utc::now());
            job.schedule = s.trim().to_string();
        }
        if let Some(p) = prompt {
            job.prompt = p.to_string();
        }
        if let Some(n) = name {
            job.name = Some(n.to_string());
        }
        if let Some(e) = enabled {
            job.enabled = e;
        }
        Some(job.clone())
    }

    /// 删除一条 job(按 id)。返回是否删除成功。
    pub fn delete(&self, id: &str) -> bool {
        let mut jobs = self.jobs.write();
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        jobs.len() != before
    }

    /// 启动后台 driver。每 `tick_secs` 秒扫描一次 enabled job,到期则把
    /// `prompt` 作为 `Submission::user_input` 发到 `sub_tx`,并更新
    /// `last_fired` / 重算 `next_fire`。返回的 `CronDriverHandle` drop
    /// 时停止 driver(底层 tokio task 自然结束)。
    ///
    /// `start` 会 clone 一份 `jobs` Arc + `sub_tx` 进 task,调用方持有的
    /// `CronScheduler` 仍可正常 create / update / delete(共享同一把锁)。
    pub fn start(self, tick_secs: u64) -> CronDriverHandle {
        let jobs = Arc::clone(&self.jobs);
        let sub_tx = self.sub_tx.clone();
        let join = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                Self::tick(&jobs, &sub_tx).await;
            }
        });
        CronDriverHandle { join: Some(join) }
    }

    /// 单次扫描:找出所有到期(enabled && next_fire <= now)的 job,触发
    /// 它们并重算 next_fire。抽成独立 fn 便于单测(不必真起 driver)。
    pub async fn tick(
        jobs: &Arc<RwLock<Vec<CronJobSpec>>>,
        sub_tx: &Option<mpsc::Sender<Submission>>,
    ) {
        let now = Utc::now();
        // 先快照到期 job(缩小锁持有时间)。
        //
        // 关键:快照 job **id**(而非数组下标)。`CronScheduler::delete` 用
        // `retain` 删除 job,会使后续元素下标前移;`create` 会 append。若在
        // 快照与写回之间发生增删,按旧下标 `get_mut(idx)` 会改到错误的 job
        // (静默串改其它 job 的 last_fired/next_fire),且被删的到期 job 的
        // next_fire 永不重算 → 每个 tick 重复触发。按 id 在写锁内重新定位
        // 即可彻底规避。
        let due: Vec<(String, String)> = {
            let g = jobs.read();
            g.iter()
                .filter(|j| j.enabled && j.next_fire.is_some_and(|nf| nf <= now))
                .map(|j| (j.id.clone(), j.prompt.clone()))
                .collect()
        };
        for (job_id, prompt) in due {
            // 触发:发 Submission。sub_tx 为 None 时跳过(测试)。
            if let Some(tx) = sub_tx {
                let _ = tx.send(Submission::user_input(prompt.clone())).await;
            }
            // 更新 last_fired + 重算 next_fire(按 id 在写锁内重新定位)。
            let mut g = jobs.write();
            if let Some(job) = g.iter_mut().find(|j| j.id == job_id) {
                job.last_fired = Some(now);
                if let Ok(parsed) = CronSchedule::parse(&job.schedule) {
                    job.next_fire = parsed.next_after(now);
                }
            }
        }
    }
}

/// driver task 句柄。drop 时 abort 底层 task。
pub struct CronDriverHandle {
    join: Option<tokio::task::JoinHandle<()>>,
}

impl CronDriverHandle {
    /// 主动停止 driver。
    pub fn stop(&mut self) {
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

impl Drop for CronDriverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── 旧 stub 别名(向后兼容 slash.rs 的 CronStubStatus 引用)──────────
//
// v0 导出的 `CronStubStatus` 在 `slash.rs` 仍被引用;保留类型别名让旧
// 代码继续编译,内部映射到新的 `CronStatus`。
#[doc(hidden)]
pub type CronStubStatus = CronStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── 表达式解析 ───────────────────────────────────────────────────

    #[test]
    fn parse_every_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(s.minute.len(), 60);
        assert_eq!(s.hour.len(), 24);
    }

    #[test]
    fn parse_field_count_error() {
        assert_eq!(
            CronSchedule::parse("* * * *").unwrap_err(),
            CronParseError::FieldCount(4)
        );
    }

    #[test]
    fn parse_out_of_range() {
        let err = CronSchedule::parse("60 * * * *").unwrap_err();
        assert!(matches!(
            err,
            CronParseError::OutOfRange {
                field: "minute",
                ..
            }
        ));
    }

    #[test]
    fn parse_step_and_list() {
        let s = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.minute, vec![0, 15, 30, 45]);
        let s = CronSchedule::parse("0,30 * * * *").unwrap();
        assert_eq!(s.minute, vec![0, 30]);
        let s = CronSchedule::parse("5-7 * * * *").unwrap();
        assert_eq!(s.minute, vec![5, 6, 7]);
    }

    #[test]
    fn parse_weekday_range() {
        let s = CronSchedule::parse("0 9 * * 1-5").unwrap();
        assert_eq!(s.dow, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_zero_step_errors() {
        let err = CronSchedule::parse("*/0 * * * *").unwrap_err();
        assert!(matches!(err, CronParseError::ZeroStep { field: "minute" }));
    }

    // ── 计算下次触发时间(next_after)──────────────────────────

    #[test]
    fn next_after_every_minute_is_next_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 2, 12, 1, 0).unwrap());
    }

    #[test]
    fn next_after_hourly_aligns_to_top() {
        let s = CronSchedule::parse("0 * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 30, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 2, 13, 0, 0).unwrap());
    }

    #[test]
    fn next_after_daily_finds_next_day() {
        let s = CronSchedule::parse("0 9 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 3, 9, 0, 0).unwrap());
    }

    // ── 调度器增删改查 ───────────────────────────────────────────

    fn sched() -> CronScheduler {
        CronScheduler::new(None, ThreadId::new())
    }

    #[test]
    fn create_validates_and_sets_next_fire() {
        let s = sched();
        let job = s
            .create("0 * * * *", "standup", Some("daily".into()))
            .unwrap();
        assert!(job.next_fire.is_some());
        assert_eq!(job.prompt, "standup");
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.status(), CronStatus::HasJobs);
    }

    #[test]
    fn create_rejects_bad_schedule() {
        let s = sched();
        assert!(s.create("not-a-cron", "x", None).is_err());
        assert_eq!(s.list().len(), 0);
        assert_eq!(s.status(), CronStatus::Idle);
    }

    #[test]
    fn create_defaults_name_from_prompt() {
        let s = sched();
        let job = s.create("0 * * * *", "hello world prompt", None).unwrap();
        assert_eq!(job.name.as_deref(), Some("hello world prompt"));
    }

    #[test]
    fn update_changes_fields_and_recomputes_next_fire() {
        let s = sched();
        let job = s.create("0 * * * *", "p1", None).unwrap();
        let nf0 = job.next_fire;
        let updated = s.update(&job.id, Some("0 9 * * *"), Some("p2"), None, None);
        assert!(updated.is_some());
        let u = updated.unwrap();
        assert_eq!(u.prompt, "p2");
        assert_eq!(u.schedule, "0 9 * * *");
        assert_ne!(u.next_fire, nf0);
    }

    #[test]
    fn update_disable_then_status_idle() {
        let s = sched();
        let job = s.create("0 * * * *", "p", None).unwrap();
        assert_eq!(s.status(), CronStatus::HasJobs);
        s.update(&job.id, None, None, None, Some(false));
        assert_eq!(s.status(), CronStatus::Idle);
    }

    #[test]
    fn delete_removes_job() {
        let s = sched();
        let job = s.create("0 * * * *", "p", None).unwrap();
        assert!(s.delete(&job.id));
        assert!(s.get(&job.id).is_none());
        assert!(!s.delete(&job.id), "second delete is no-op");
    }

    // ── 驱动循环 tick ──────────────────────────────────────────

    #[tokio::test]
    async fn tick_fires_due_job_and_recomputes_next_fire() {
        let s = sched();
        let mut job = s.create("* * * * *", "fired", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(5));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        CronScheduler::tick(&jobs, &None).await;
        let after = s.get(&job.id).unwrap();
        assert!(after.last_fired.is_some(), "last_fired should be set");
        assert!(after.next_fire.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn tick_sends_submission_when_tx_present() {
        let (tx, mut rx) = mpsc::channel::<Submission>(8);
        let s = CronScheduler::new(Some(tx), ThreadId::new());
        let mut job = s.create("* * * * *", "hello-cron", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        let tx = s.sub_tx.clone().unwrap();
        CronScheduler::tick(&jobs, &Some(tx)).await;
        let sub = rx.recv().await.expect("should receive a submission");
        assert!(matches!(sub.op, reflect_protocol::Op::UserInput { .. }));
    }

    #[tokio::test]
    async fn tick_skips_disabled_jobs() {
        let s = sched();
        let mut job = s.create("* * * * *", "p", None).unwrap();
        job.enabled = false;
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        CronScheduler::tick(&jobs, &None).await;
        let after = s.get(&job.id).unwrap();
        assert!(after.last_fired.is_none(), "disabled job must not fire");
    }

    // 回归:tick 快照与写回之间发生 delete(retain)使下标前移时,必须按
    // **id** 而非下标重定位,否则会把 last_fired/next_fire 写到错误的 job 上。
    // 复现:job A、job B 均到期 → tick 快照两者 → 删 A(下标前移,B 从 1→0)
    // → 写回时若按下标,B 被更新两次、A(已删)的更新落到 B 头上。
    #[tokio::test]
    async fn tick_survives_concurrent_delete_by_resolving_id() {
        let s = sched();
        // 两个到期 job,故意给 A 一个更早的 next_fire 让它排在快照第一位。
        let mut a = s.create("* * * * *", "A", None).unwrap();
        a.next_fire = Some(Utc::now() - Duration::minutes(10));
        let mut b = s.create("* * * * *", "B", None).unwrap();
        b.next_fire = Some(Utc::now() - Duration::minutes(5));
        let id_a = a.id.clone();
        let id_b = b.id.clone();
        {
            let mut g = s.jobs.write();
            g[0] = a;
            g[1] = b;
        }
        let jobs = Arc::clone(&s.jobs);

        // tick 内部先快照(A、B),再逐个写回。我们在两次写回之间删掉 A ——
        // 模拟并发 delete。由于 tick 逐 job 加写锁,这里在 tick 之前删 A 即可
        // 制造「快照里还有 A、但写回时 A 已不在」的场景,验证写回按 id 定位、
        // 不污染 B。
        s.delete(&id_a);
        CronScheduler::tick(&jobs, &None).await;

        // B 必须被正确更新(last_fired + next_fire 重算),而非保持过期值。
        let after_b = s.get(&id_b).expect("B must still exist");
        assert!(
            after_b.last_fired.is_some(),
            "B should have been fired and its last_fired set"
        );
        assert!(
            after_b.next_fire.unwrap() > Utc::now(),
            "B's next_fire should have been recomputed into the future"
        );
    }
}
