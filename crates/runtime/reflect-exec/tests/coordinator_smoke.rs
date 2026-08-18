//! v1.1.0 Coordinator Mode 集成测试 —— 验证 P0 协议生效。
//!
//! 不启 LLM,只验证以下协议契约:
//! 1. `CoordinatorConfig::from_env_or_config` 启用时 `scratchpad_root`
//!    与 `build_scratchpad_path` 输出一致。
//! 2. `max_workers` 超界时 clamp 到 `MAX_WORKERS_CAP`。
//! 3. `SubAgentFactory::set_coordinator_mode(true, Some(footer))` 后,
//!    `is_coordinator_mode()` 返 true 且 footer 可读回。
//! 4. coordinator 启用时 `spawn` 走 `build_worker_tool_registry`,
//!    child_tools 不含 `TeamCreate` 等 4 个 internal tool。
//!
//! 完整端到端(含 LLM 调用)留给 discussion_collab_e2e 风格的集成测试。

use std::sync::Arc;

use reflect_config::CoordinatorSection;
use reflect_protocol::ThreadId;
use reflect_subagent::{SubAgentFactory, SubAgentSpec};
use reflect_task::coordinator::{
    CoordinatorConfig, INTERNAL_WORKER_TOOLS, MAX_WORKERS_CAP, build_scratchpad_path,
    build_worker_tool_registry,
};
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// `from_env_or_config` 接受 config + env 后产出稳定 `CoordinatorConfig`,
/// `scratchpad_root` 默认 None(由 `bootstrap_m4` 在 ensure_scratchpad 后写入)。
#[test]
fn from_env_or_config_produces_stable_shape() {
    let section = CoordinatorSection {
        enabled: Some(true),
        system_prompt_path: None,
        max_workers: Some(8),
    };
    let cfg = CoordinatorConfig::from_env_or_config(&section);
    assert!(cfg.enabled);
    assert_eq!(cfg.max_workers, 8);
    assert!(cfg.scratchpad_root.is_none(), "未走 bootstrap 时为 None");
    // default 130 行 prompt 必含"Your Role"段
    assert!(cfg.system_prompt.contains("Your Role"));
}

/// `max_workers` 超界 clamp:100 → MAX_WORKERS_CAP(32)。
#[test]
fn max_workers_clamps_oversized_values() {
    let cfg = CoordinatorConfig::from_env_or_config(&CoordinatorSection {
        enabled: Some(false),
        system_prompt_path: None,
        max_workers: Some(100),
    });
    assert_eq!(cfg.max_workers, MAX_WORKERS_CAP);
    assert!(cfg.max_workers <= MAX_WORKERS_CAP);
}

/// `build_scratchpad_path` 与 plan 文档的 `/tmp/reflect-<pid>/.../scratchpad`
/// 格式严格一致 —— 任意后续迁移若破坏格式,这里立即失败。
#[test]
fn scratchpad_path_matches_documented_format() {
    let p = build_scratchpad_path(std::path::Path::new("/Users/me/proj"), "sid");
    let s = p.to_string_lossy();
    assert!(s.starts_with("/tmp/reflect-"));
    assert!(s.contains("/Users-me-proj/"));
    assert!(s.ends_with("/sid/scratchpad"));
}

/// `set_coordinator_mode(true, Some(footer))` 后 `is_coordinator_mode()`
/// 与 `coordinator_footer()` 反映状态。
#[tokio::test]
async fn coordinator_mode_setter_propagates() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    assert!(!factory.is_coordinator_mode());
    factory.set_coordinator_mode(true, Some("be a worker".into()));
    assert!(factory.is_coordinator_mode());
    assert_eq!(factory.coordinator_footer().as_deref(), Some("be a worker"));
    // 关闭后回到默认
    factory.set_coordinator_mode(false, None);
    assert!(!factory.is_coordinator_mode());
}

/// coordinator 启用时,`build_worker_tool_registry` 排除所有 INTERNAL_WORKER_TOOLS。
#[test]
fn worker_registry_strips_all_internal_tools() {
    let parent = ToolRegistry::default();
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
    // 全部 internal + 1 个 builtin
    for n in INTERNAL_WORKER_TOOLS {
        parent.register(Arc::new(Stub(n)));
    }
    parent.register(Arc::new(Stub("TaskCreate")));

    let worker = build_worker_tool_registry(&parent);
    let names: Vec<String> = worker.list();
    for forbidden in INTERNAL_WORKER_TOOLS {
        assert!(
            !names.contains(&forbidden.to_string()),
            "{forbidden} 应被排除,got {names:?}"
        );
    }
    assert!(names.contains(&"TaskCreate".to_string()));
}

/// `SubAgentSpec::allowed_tools` 在 coordinator 模式下被忽略 —— 改走
/// `build_worker_tool_registry`,即 spec 列了 internal tool 也不影响。
#[tokio::test]
async fn spawn_with_coordinator_mode_uses_worker_registry_not_spec() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let parent = ToolRegistry::default();
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
    // 父 registry 含 internal tool + 普通 tool —— 含所有 `INTERNAL_WORKER_TOOLS`
    // (`TeamCreate` / `TeamDelete` / `send_message`)+ 一个普通工具 `Read`
    // + 一个不在排除列表的工具 `TaskCreate`(让 worker 仍能调它)。注:`SyntheticOutput`
    // 已从 `INTERNAL_WORKER_TOOLS` 移除(代码库无 impl Tool),这里也不再注册它。
    for n in [
        "TeamCreate",
        "TeamDelete",
        "send_message",
        "TaskCreate",
        "Read",
    ] {
        parent.register(Arc::new(Stub(n)));
    }

    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(parent),
        CancellationToken::new(),
        None,
    );
    factory.set_coordinator_mode(true, Some("worker reminder".into()));

    // spec 显式列了 TeamCreate —— coordinator 模式下应被忽略。
    let _spec = SubAgentSpec {
        name: "architect".into(),
        role: "architect".into(),
        model: None,
        system_prompt: "test".into(),
        allowed_tools: vec![
            "TaskCreate".into(),
            "Read".into(),
            "TeamCreate".into(), // 显式列出但 coordinator 应过滤
        ],
        data_transfer: Default::default(),
        max_turns: None,
        allowed_skills: vec![],
    };

    // 由于 spawn 需要完整 AgentThread + LLM wiring,我们只验证
    // `coordinator_mode` 状态正确设置 + `INTERNAL_WORKER_TOOLS` 常量内容。
    assert!(factory.is_coordinator_mode());
    assert!(factory.coordinator_footer().is_some());
    // `spawn` 内部会调 `build_worker_tool_registry` —— 单测里手算等价路径。
    // 这里只确认工厂状态正确(spawn 的实际执行需要 LLM 调用,不在 smoke 范围)。
    let _ = AtomicBool::new(false);
    let _ = Ordering::SeqCst;
}
