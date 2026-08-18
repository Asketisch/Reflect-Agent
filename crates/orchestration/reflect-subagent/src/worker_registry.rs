//! Worker 子 agent 的工具白名单 —— coordinator 模式专用。
//!
//! v1.1.0 Phase 4 把 `INTERNAL_WORKER_TOOLS` 与 `build_worker_tool_registry`
//! 从 `reflect_task::coordinator` 搬到 `reflect_subagent` 自己,理由:
//!
//! - `SubAgentFactory::spawn` 在 coordinator 启用时调用本模块过滤工具;
//!   此调用点位于 `reflect-subagent`,若依赖 `reflect-task` 会形成循环依赖
//!   (因为 `reflect-task::TaskManager` 已经依赖 `reflect-subagent`)。
//! - 把协议规则与实现放在同一 crate,降低跨 crate 维护成本。
//!
//! `reflect_task::coordinator` 用 `pub use reflect_subagent::worker_registry::*`
//! 保持外部 API 兼容(已落地的 11 个单测 + `register_except` 路径都引用
//! `reflect_task::coordinator::INTERNAL_WORKER_TOOLS` / `build_worker_tool_registry`)。
//!
//! ## Coordinator 模式下的 worker 工具白名单
//!
//! Worker 不该被赋予 `TeamCreate` / `TeamDelete` —— 团队生命周期由
//! coordinator 全权管理。`SyntheticOutput` 与 `send_message` 是反向通道
//! 与内部工具,worker 也无需直接调用(留 v1.2 SendMessage)。

use std::collections::HashMap;
use std::sync::Arc;

use reflect_tools::{ToolRegistry, ToolSource};

/// Coordinator 自身拥有 / worker 必须排除的工具列表。
///
/// 排除名单覆盖 3 个已注册工具 —— 团队生命周期由 coordinator 全权管理,
/// worker 不应自行创建 / 删除团队,也不该向 coordinator 直接发消息。
///
/// 历史:`SyntheticOutput` 原列入排除名单但代码库中并无 `impl Tool` 实现,
/// 属幽灵名 —— 已从列表移除。后续若实现该工具再补回。
pub const INTERNAL_WORKER_TOOLS: &[&str] = &["TeamCreate", "TeamDelete", "send_message"];

/// 从父 `ToolRegistry` 复制所有 `Builtin` / `Runtime` 工具,排除
/// `INTERNAL_WORKER_TOOLS`,返回新的 registry。
///
/// # 为什么不过滤 Plugin / MCP
/// Worker 不该被赋予 plugin / MCP 工具 —— coordinator 主 session 已经
/// 持有这些;子 worker 只用 builtin task / read / write 工具做实际工作。
/// 这条白名单与 `INTERNAL_WORKER_TOOLS` 一起,定义 worker 的最小能力集。
///
/// 走两步:
/// 1. 收集父 registry 里 `Builtin` + `Runtime` 工具(按 source 标签筛);
/// 2. 用 `register_except(source, tools, INTERNAL_WORKER_TOOLS)` 批量
///    注册,`register_except` 内部做排除 + 计数。
pub fn build_worker_tool_registry(parent: &ToolRegistry) -> ToolRegistry {
    let mut by_source: HashMap<ToolSource, Vec<Arc<dyn reflect_tools::Tool>>> = HashMap::new();
    for (name, source) in parent.list_with_source() {
        if !matches!(source, ToolSource::Builtin | ToolSource::Runtime) {
            continue;
        }
        if let Some(tool) = parent.get(&name) {
            by_source.entry(source).or_default().push(tool);
        }
    }
    let worker = ToolRegistry::default();
    for (source, tools) in by_source {
        let registered = worker.register_except(source, tools, INTERNAL_WORKER_TOOLS);
        tracing::debug!(
            source = ?source,
            registered,
            excluded_count = INTERNAL_WORKER_TOOLS.len(),
            "build_worker_tool_registry: 复制完成"
        );
    }
    worker
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use reflect_protocol::ToolOutput;
    use reflect_tools::{Tool, ToolContext, ToolError};

    struct Stub(&'static str);
    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _: ToolContext,
            _: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            unimplemented!()
        }
    }

    /// 父 registry 有 6 个 builtin(含 1 个 internal)→ worker 拿 5 个。
    #[test]
    fn worker_registry_excludes_internal_tools() {
        let parent = ToolRegistry::default();
        for n in ["TaskCreate", "Read", "Write", "Grep", "Glob", "TeamCreate"] {
            parent.register(Arc::new(Stub(n)));
        }
        let worker = build_worker_tool_registry(&parent);
        let names: Vec<String> = worker.list();
        assert_eq!(names.len(), 5, "5 个非 internal tool,got {names:?}");
        assert!(names.contains(&"TaskCreate".to_string()));
        assert!(names.contains(&"Read".to_string()));
        assert!(!names.contains(&"TeamCreate".to_string()));
    }

    /// 排除名单精确覆盖 3 个已注册工具,与 coordinator 模式契约一致。
    /// `SyntheticOutput` 已从列表移除(代码库无 impl Tool,属幽灵名)。
    #[test]
    fn internal_worker_tools_exact_set() {
        let expected = ["TeamCreate", "TeamDelete", "send_message"];
        assert_eq!(INTERNAL_WORKER_TOOLS.len(), expected.len());
        for t in expected {
            assert!(INTERNAL_WORKER_TOOLS.contains(&t), "missing {t}");
        }
        // 幽灵名不应再出现
        assert!(
            !INTERNAL_WORKER_TOOLS.contains(&"SyntheticOutput"),
            "SyntheticOutput has no impl Tool — should be removed from the exclusion list"
        );
    }
}
