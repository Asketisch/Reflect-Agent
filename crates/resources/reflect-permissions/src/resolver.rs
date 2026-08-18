//! `resolver` —— `PermissionResolver` trait + 内存 + store-backed 实现。
//!
//! resolver 是 ApprovalGate 的依赖:gate 在 `ask_tool` 短路 Allow / Deny,
//! 通过 resolver 查"这个 tool 当前有没有显式规则"。

use std::sync::Arc;

use crate::rules::RuleMatch;
use crate::store::PermissionStore;

/// Resolver trait。`async` 简化未来扩展(network / db 后端)。
///
/// 设计取舍:`async fn` 而不是 `fn(&self) -> RuleMatch` —— 即使当前
/// 两个 impl(File + InMemory)都是 sync,async 让 store 端可以走
/// `tokio::fs` 而不阻塞 reactor。代价是 caller 多一层 `.await`,但
/// `ask_tool` 本来就是 async 函数,无影响。
///
/// 两个方法:
/// - [`resolve`](Self::resolve):无上下文,只按工具名匹配。用于无 bash
///   命令可参考的路径(如 `ask_user_opts`)。default 实现转发到
///   [`resolve_with_context`](Self::resolve_with_context) 传 `None`。
/// - [`resolve_with_context`](Self::resolve_with_context):带 bash 命令
///   上下文,让 `shell_pattern` 规则(如 `Bash: git *`)能命中。**有 bash
///   命令的调用点(如 `ApprovalGate::ask_tool`)应优先调它**,否则
///   `shell_pattern` 规则静默失效。
#[async_trait::async_trait]
pub trait PermissionResolver: Send + Sync {
    /// 无 bash 上下文的解析(default 实现):转发到 `resolve_with_context`
    /// 传 `None`。仅当 caller 确实没有 bash 命令时用(如非 Bash 工具、或
    /// ask_user 路径)。
    async fn resolve(&self, tool_name: &str) -> RuleMatch {
        self.resolve_with_context(tool_name, None).await
    }

    /// 带 bash 命令上下文的解析。`bash_command = Some(cmd)` 时,
    /// `shell_pattern` 规则(仅 `Bash`/`bash` 工具)参与匹配;`None` 时
    /// 跳过 shell 规则(与旧 `resolve` 行为一致)。
    async fn resolve_with_context(&self, tool_name: &str, bash_command: Option<&str>) -> RuleMatch;
}

// ── StorePermissionResolver ─────────────────────────────────────────────

/// 把 `PermissionStore` 包成 resolver。**错误降级**:list 失败
/// (corrupt toml / IO 错误)→ `NoMatch`,让上层按 `PermissionMode`
/// 默认行为走,而不是 deny 用户工作流。
///
/// "降级而非 fail-closed"的理由:用户误编辑 toml 不该让所有 tool 调用
/// 失败。`tracing::warn!` 留排查痕迹。
pub struct StorePermissionResolver {
    store: Arc<dyn PermissionStore>,
}

// 手写 Debug:`Arc<dyn PermissionStore>` 没有 derive Debug。
impl std::fmt::Debug for StorePermissionResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePermissionResolver")
            .field("store", &"<dyn PermissionStore>")
            .finish()
    }
}

impl StorePermissionResolver {
    pub fn new(store: Arc<dyn PermissionStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl PermissionResolver for StorePermissionResolver {
    async fn resolve_with_context(&self, tool_name: &str, bash_command: Option<&str>) -> RuleMatch {
        match self.store.list().await {
            Ok(rules) => crate::matcher::evaluate_with_context(&rules, tool_name, bash_command),
            Err(e) => {
                tracing::warn!(error = %e, tool = %tool_name, "permission store list failed; falling back to NoMatch");
                RuleMatch::NoMatch
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{PermissionAction, PermissionRule};
    use crate::store::InMemoryPermissionStore;

    #[tokio::test]
    async fn resolver_returns_allow_when_rule_present() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::Allow);
    }

    #[tokio::test]
    async fn resolver_returns_deny_when_rule_present() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Write".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Write").await, RuleMatch::Deny);
    }

    #[tokio::test]
    async fn resolver_returns_no_match_for_unknown_tool() {
        let s = Arc::new(InMemoryPermissionStore::new());
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::NoMatch);
    }

    #[tokio::test]
    async fn resolve_with_context_matches_shell_pattern() {
        // shell_pattern 规则在 bash_command 传入时应命中(这是此前 bug 的回归测试:
        // 旧 resolve 只调 evaluate 无 bash 上下文,shell_pattern 静默失效)。
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        // 传匹配的 bash 命令 → Allow。
        assert_eq!(
            r.resolve_with_context("Bash", Some("git status")).await,
            RuleMatch::Allow,
            "shell_pattern='git *' 应匹配 'git status'"
        );
        // 不匹配的命令 → NoMatch(回退默认审批)。
        assert_eq!(
            r.resolve_with_context("Bash", Some("rm -rf /")).await,
            RuleMatch::NoMatch,
            "shell_pattern='git *' 不应匹配 'rm -rf /'"
        );
        // 无 bash 上下文 → shell_pattern 规则被跳过 → NoMatch。
        assert_eq!(
            r.resolve_with_context("Bash", None).await,
            RuleMatch::NoMatch,
            "无 bash 上下文时 shell_pattern 规则不参与"
        );
    }

    #[tokio::test]
    async fn resolve_default_forwards_none_context() {
        // 默认 resolve() 应转发 resolve_with_context(_, None),
        // 故 shell_pattern 规则不参与(与上面 None 用例一致)。
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::NoMatch);
    }
}
