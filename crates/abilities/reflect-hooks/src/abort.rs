//! `HookAbortSignal` —— hook + tool 执行链路的协作式取消信号。
//!
//! 基于 `Arc<AtomicBool>` 的协作取消模式。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 廉价克隆的取消标志,贯穿整条 hook+tool 流水线。
#[derive(Debug, Clone, Default)]
pub struct HookAbortSignal {
    inner: Arc<AtomicBool>,
}

impl HookAbortSignal {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 标记信号为已触发,幂等。
    pub fn trigger(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    /// 当 [`trigger`](Self::trigger) 已被调用时返回 `true`。
    pub fn is_triggered(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    /// 重置为未触发状态(用于 turn 之间,通常不必调用)。
    pub fn reset(&self) {
        self.inner.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_untriggered() {
        let s = HookAbortSignal::new();
        assert!(!s.is_triggered());
    }

    #[test]
    fn trigger_sets_flag() {
        let s = HookAbortSignal::new();
        s.trigger();
        assert!(s.is_triggered());
    }

    #[test]
    fn clones_share_state() {
        let s = HookAbortSignal::new();
        let c = s.clone();
        c.trigger();
        assert!(s.is_triggered());
    }

    #[test]
    fn reset_clears_flag() {
        let s = HookAbortSignal::new();
        s.trigger();
        s.reset();
        assert!(!s.is_triggered());
    }
}
