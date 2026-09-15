//! `SubagentRuntimeRegistry` — 运行中子代理的取消令牌注册表。
//!
//! 与 reflect-recovery 的 `SubagentRegistry`(已完成子代理调用的
//! 记录缓存,防重复 spawn)是两个正交设施:本表只跟踪**在飞**子代理,
//! 支撑 `Op::Interrupt { child_id }` 的定向中断 —— key 是子代理的
//! `ThreadId` 字符串形态(与 `SpawnedChild.session_id` 一致)。
//!
//! 生命周期:`SubAgentFactory::spawn` 在构造子 `AgentConfig` 前登记,
//! `SpawnedChild` 终态(collect 完成 / Drop 提前放弃)时注销。父会话
//! `Op::Shutdown` 不需要经本表 —— 子代理的 cancel 令牌派生自父会话
//! 令牌(child_token),会话级取消自动级联。
//!
//! v1.4 A1 为最小版(仅取消令牌);计划中的子代理状态中心(角色 /
//! 迭代 / 工具 / token 快照)将在编排阶段把 value 升级为完整结构体。

use std::collections::HashMap;

use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

/// 会话级「子代理会话号 → 取消令牌」映射。
///
/// `RwLock` 而非 `Mutex`:spawn / 注销走写锁,`Op::Interrupt` 路由与
/// 诊断查询走读锁,多读者并发安全。`Arc` 包裹由调用方完成(父
/// `AgentConfig` 与 `SubAgentFactory` 共享同一实例)。
#[derive(Default)]
pub struct SubagentRuntimeRegistry {
    inner: RwLock<HashMap<String, CancellationToken>>,
}

impl SubagentRuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个在飞子代理。同 id 重复登记时后者覆盖(旧令牌失效,
    /// 与其关联的子代理若仍在跑将无法再被定向中断 —— spawn 前的
    /// `ThreadId::new` 保证唯一,此分支仅防御性存在)。
    pub fn register(&self, child_id: impl Into<String>, cancel: CancellationToken) {
        self.inner.write().insert(child_id.into(), cancel);
    }

    /// 注销一个子代理条目。返回其令牌(调用方可决定是否顺带取消)。
    pub fn unregister(&self, child_id: &str) -> Option<CancellationToken> {
        self.inner.write().remove(child_id)
    }

    /// 定向中断:取消指定子代理的令牌并移除条目。
    /// 返回 `false` 表示没有该 id 的在飞条目(未知 / 已结束)。
    pub fn cancel_child(&self, child_id: &str) -> bool {
        match self.unregister(child_id) {
            Some(tok) => {
                tok.cancel();
                true
            }
            None => false,
        }
    }

    /// 当前在飞子代理的会话号快照(诊断 / 后续状态查询用)。
    pub fn child_ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }

    /// 在飞子代理数量。
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

    /// 登记 → 定向取消命中,条目随之移除。
    #[tokio::test]
    async fn cancel_child_cancels_and_removes_entry() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        reg.register("child-1", tok.clone());
        assert_eq!(reg.len(), 1);

        assert!(reg.cancel_child("child-1"));
        assert!(tok.is_cancelled(), "定向中断必须触发对应令牌");
        assert!(reg.is_empty(), "取消后条目应被移除");
    }

    /// 未知 id 的定向中断返回 false,不影响其他条目。
    #[tokio::test]
    async fn cancel_unknown_child_is_noop() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        reg.register("child-1", tok.clone());
        assert!(!reg.cancel_child("ghost"));
        assert!(!tok.is_cancelled());
        assert_eq!(reg.len(), 1, "误报不应影响既有条目");
    }

    /// 注销只移除条目,不触发取消(collect 正常完成路径)。
    #[tokio::test]
    async fn unregister_keeps_token_intact() {
        let reg = SubagentRuntimeRegistry::new();
        let tok = CancellationToken::new();
        reg.register("child-1", tok.clone());
        let taken = reg.unregister("child-1").expect("条目应在");
        assert!(!taken.is_cancelled());
        assert!(!tok.is_cancelled(), "正常完成不应取消令牌");
    }
}
