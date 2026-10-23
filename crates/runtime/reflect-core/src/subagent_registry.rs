//! `SubagentRuntimeRegistry` — 子代理状态中心(在飞 + 近期终态)。
//!
//! 与 reflect-recovery 的 `SubagentRegistry`(已完成子代理调用的
//! 记录缓存,防重复 spawn)是两个正交设施。本表服务三件事:
//!
//! 1. **定向中断**:`Op::Interrupt { child_id }` 查表取消对应令牌;
//! 2. **状态查询**:`Op::QuerySubagents` 读快照应答(读取方拿 clone,
//!    永不阻塞子代理本身);
//! 3. **进度推送底座**:`SpawnedChild` 经 cancel 令牌与槽位生命周期
//!    对齐(collect 完成 / Drop 放弃时槽位转终态)。
//!
//! 写入模型:spawn 时 `register` 建槽;子代理线程在关键节点(工具
//! 开始/结束、回合结束)向自己的槽写快照字段 —— 槽内部 `Mutex` 保护,
//! 写者只有子代理自己,读者任意多。
//!
//! 终态保留:完成/失败/取消**不立即删**,槽位标记终态并保留
//! `retention`(默认 5 分钟)供事后查询;下次 `register` 时顺带清扫
//! 过期条目(无独立后台任务,零开销)。

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use reflect_protocol::{SubagentRunStateMirror, SubagentStatusSnapshot};

/// 终态条目的默认保留时长。
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(5 * 60);

/// 单个子代理的共享状态槽。
pub struct SubagentStatusSlot {
    child_id: String,
    cancel: CancellationToken,
    inner: Mutex<SlotInner>,
}

#[derive(Debug, Clone)]
struct SlotInner {
    role: String,
    started_at: chrono::DateTime<chrono::Utc>,
    state: SubagentRunStateMirror,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    iteration: u32,
    current_tool: Option<String>,
    total_tokens: u64,
    last_event: Option<String>,
}

impl SubagentStatusSlot {
    /// 当前快照(全字段克隆,供协议应答)。
    pub fn snapshot(&self) -> SubagentStatusSnapshot {
        let g = self.inner.lock();
        SubagentStatusSnapshot {
            child_id: self.child_id.clone(),
            role: g.role.clone(),
            state: g.state,
            started_at: g.started_at,
            finished_at: g.finished_at,
            iteration: g.iteration,
            current_tool: g.current_tool.clone(),
            total_tokens: g.total_tokens,
            last_event: g.last_event.clone(),
        }
    }

    /// 取消令牌(父级 `Op::Interrupt { child_id }` 与 Drop 放弃路径用)。
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// 子代理会话号。
    pub fn child_id(&self) -> &str {
        &self.child_id
    }

    // ── 子代理侧写入(自己写自己的槽) ──────────────────────────

    /// 更新迭代计数(每回合结束时)。
    pub fn set_iteration(&self, iteration: u32) {
        self.inner.lock().iteration = iteration;
    }

    /// 累加 token 用量。
    pub fn add_tokens(&self, total_tokens: u64) {
        self.inner.lock().total_tokens += total_tokens;
    }

    /// 标记工具开始。
    pub fn begin_tool(&self, tool: &str) {
        let mut g = self.inner.lock();
        g.current_tool = Some(tool.to_string());
        g.last_event = Some(truncate_event(&format!("tool begin: {tool}")));
    }

    /// 标记工具结束。
    pub fn end_tool(&self, tool: &str) {
        let mut g = self.inner.lock();
        g.current_tool = None;
        g.last_event = Some(truncate_event(&format!("tool end: {tool}")));
    }

    /// 追加一条自由文本事件摘要(进度转发侧可选写入)。
    pub fn note_event(&self, summary: &str) {
        self.inner.lock().last_event = Some(truncate_event(summary));
    }

    /// 转终态。`Running` 之外的重复设置被忽略(首个终态为准,
    /// 防止 collect 与 Drop 竞争互覆)。
    pub fn finish(&self, state: SubagentRunStateMirror) {
        let mut g = self.inner.lock();
        if g.state != SubagentRunStateMirror::Running {
            return;
        }
        g.state = state;
        g.finished_at = Some(chrono::Utc::now());
        g.current_tool = None;
    }

    /// 是否仍处 Running(供 Drop 路径决定是否标记 Cancelled)。
    pub fn is_running(&self) -> bool {
        self.inner.lock().state == SubagentRunStateMirror::Running
    }
}

/// 事件摘要截断(防异常长文本撑爆快照)。
fn truncate_event(s: &str) -> String {
    const CAP: usize = 200;
    if s.len() <= CAP {
        s.to_string()
    } else {
        let mut cut = CAP;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

/// 会话级子代理状态中心。
pub struct SubagentRuntimeRegistry {
    inner: RwLock<HashMap<String, std::sync::Arc<SubagentStatusSlot>>>,
    retention: Duration,
}

impl Default for SubagentRuntimeRegistry {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            retention: DEFAULT_RETENTION,
        }
    }
}

impl SubagentRuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 自定义终态保留时长(测试用)。
    pub fn with_retention(retention: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            retention,
        }
    }

    /// 登记一个在飞子代理并建状态槽。顺带清扫过期终态条目。
    pub fn register(
        &self,
        child_id: impl Into<String>,
        role: impl Into<String>,
        cancel: CancellationToken,
    ) -> std::sync::Arc<SubagentStatusSlot> {
        let child_id = child_id.into();
        let slot = std::sync::Arc::new(SubagentStatusSlot {
            child_id: child_id.clone(),
            cancel,
            inner: Mutex::new(SlotInner {
                role: role.into(),
                started_at: chrono::Utc::now(),
                state: SubagentRunStateMirror::Running,
                finished_at: None,
                iteration: 0,
                current_tool: None,
                total_tokens: 0,
                last_event: None,
            }),
        });
        let mut map = self.inner.write();
        // 清扫过期终态(仅在有新登记时触发,无后台任务)。
        let cutoff = chrono::Utc::now()
            - chrono::Duration::from_std(self.retention)
                .unwrap_or_else(|_| chrono::Duration::minutes(5));
        map.retain(|_, s| {
            let g = s.inner.lock();
            match g.finished_at {
                Some(t) => t > cutoff,
                None => true,
            }
        });
        map.insert(child_id, slot.clone());
        slot
    }

    /// 注销一个子代理条目。返回其槽位(调用方可决定是否顺带取消)。
    pub fn unregister(&self, child_id: &str) -> Option<std::sync::Arc<SubagentStatusSlot>> {
        self.inner.write().remove(child_id)
    }

    /// 定向中断:取消指定子代理的令牌并移除条目(槽位转 Cancelled ——
    /// 中断即终态,但保留在表里供事后查询)。
    /// 返回 `false` 表示没有该 id 的在飞条目(未知 / 已注销)。
    pub fn cancel_child(&self, child_id: &str) -> bool {
        match self.unregister(child_id) {
            Some(slot) => {
                slot.finish(SubagentRunStateMirror::Cancelled);
                slot.cancel.cancel();
                // 重新放回终态保留区(短窗口内 QuerySubagents 仍可查到)。
                self.inner.write().insert(child_id.to_string(), slot);
                true
            }
            None => false,
        }
    }

    /// 查询快照:`None` 列出全部(在飞 + 保留期内的终态),`Some(id)`
    /// 只查指定子代理。
    pub fn snapshot(&self, child_id: Option<&str>) -> Vec<SubagentStatusSnapshot> {
        let map = self.inner.read();
        match child_id {
            Some(id) => map.get(id).map(|s| vec![s.snapshot()]).unwrap_or_default(),
            None => {
                let mut v: Vec<_> = map.values().map(|s| s.snapshot()).collect();
                v.sort_by_key(|s| s.started_at);
                v
            }
        }
    }

    /// 当前在飞子代理的会话号快照(诊断用)。
    pub fn child_ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }

    /// 在飞条目数量。
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

impl std::fmt::Debug for SubagentRuntimeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRuntimeRegistry")
            .field("children", &self.child_ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记 → 定向取消命中:令牌触发、槽位转 Cancelled、条目保留供查询。
    #[tokio::test]
    async fn cancel_child_marks_cancelled_and_keeps_for_query() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        let slot = reg.register("child-1", "explorer", tok.clone());
        assert_eq!(reg.len(), 1);
        assert_eq!(slot.snapshot().state, SubagentRunStateMirror::Running);

        assert!(reg.cancel_child("child-1"));
        assert!(tok.is_cancelled(), "定向中断必须触发对应令牌");
        let snaps = reg.snapshot(None);
        assert_eq!(snaps.len(), 1, "取消后条目应保留供事后查询");
        assert_eq!(snaps[0].state, SubagentRunStateMirror::Cancelled);
        assert!(snaps[0].finished_at.is_some());
    }

    /// 未知 id 的定向中断返回 false,不影响其他条目。
    #[tokio::test]
    async fn cancel_unknown_child_is_noop() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        reg.register("child-1", "explorer", tok.clone());
        assert!(!reg.cancel_child("ghost"));
        assert!(!tok.is_cancelled());
        assert_eq!(reg.child_ids().len(), 1, "误报不应影响既有条目");
    }

    /// 注销只移除条目,不触发取消(collect 正常完成路径)。
    #[tokio::test]
    async fn unregister_keeps_token_intact() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        reg.register("child-1", "explorer", tok.clone());
        let slot = reg.unregister("child-1").expect("条目应在");
        assert!(!slot.cancel_token().is_cancelled());
        assert!(!tok.is_cancelled(), "正常完成不应取消令牌");
    }

    /// 槽位字段更新链:工具开始/结束、迭代、token、终态互覆保护。
    #[tokio::test]
    async fn slot_updates_and_finish_protection() {
        let reg = SubagentRuntimeRegistry::new();
        let slot = reg.register("c", "writer", CancellationToken::new());
        slot.begin_tool("bash");
        assert_eq!(slot.snapshot().current_tool.as_deref(), Some("bash"));
        slot.set_iteration(3);
        slot.add_tokens(150);
        slot.end_tool("bash");
        let s = slot.snapshot();
        assert!(s.current_tool.is_none());
        assert_eq!(s.iteration, 3);
        assert_eq!(s.total_tokens, 150);
        assert!(s.last_event.as_deref().unwrap().contains("bash"));

        slot.finish(SubagentRunStateMirror::Completed);
        slot.finish(SubagentRunStateMirror::Failed); // 首个终态为准
        assert_eq!(slot.snapshot().state, SubagentRunStateMirror::Completed);
        assert!(!slot.is_running());
    }

    /// snapshot(child_id) 定向查询;未知 id 返回空。
    #[tokio::test]
    async fn snapshot_by_child_id() {
        let reg = SubagentRuntimeRegistry::new();
        reg.register("c1", "a", CancellationToken::new());
        reg.register("c2", "b", CancellationToken::new());
        assert_eq!(reg.snapshot(Some("c1")).len(), 1);
        assert_eq!(reg.snapshot(Some("c1"))[0].role, "a");
        assert!(reg.snapshot(Some("ghost")).is_empty());
        assert_eq!(reg.snapshot(None).len(), 2);
    }

    /// 过期终态条目在下次 register 时被清扫;未过期保留。
    #[tokio::test]
    async fn register_sweeps_expired_terminal_entries() {
        let reg = SubagentRuntimeRegistry::with_retention(Duration::from_millis(50));
        let s1 = reg.register("old", "a", CancellationToken::new());
        s1.finish(SubagentRunStateMirror::Completed);
        std::thread::sleep(Duration::from_millis(80));
        // 过期终态 + 新登记 → old 被清扫。
        reg.register("new", "b", CancellationToken::new());
        let ids = reg.child_ids();
        assert!(
            !ids.contains(&"old".to_string()),
            "过期终态应被清扫: {ids:?}"
        );
        assert!(ids.contains(&"new".to_string()));
    }
}
