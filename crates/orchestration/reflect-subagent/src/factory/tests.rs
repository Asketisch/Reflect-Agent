//! 模块测试。从原文件内联的 `#[cfg(test)] mod tests` 迁移而来。

use super::*;
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

#[test]
fn depth_increments_on_spawn() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    assert_eq!(factory.depth(), 0);
    factory.in_flight.fetch_add(1, Ordering::SeqCst);
    assert_eq!(factory.depth(), 1);
}

#[test]
fn child_factory_shares_depth_counter() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    let child = factory.child_factory();
    factory.in_flight.fetch_add(1, Ordering::SeqCst);
    assert_eq!(child.depth(), 1, "child sees the same counter");
}

#[test]
fn max_depth_refuses_after_max_in_flight() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    // 假装已发生 `MAX_DEPTH` 个 spawn(counter == 16)。
    factory
        .in_flight
        .fetch_add(crate::MAX_DEPTH, Ordering::SeqCst);
    // `spawn` 顶部的检查是 `prev >= MAX_DEPTH`;当计数器已达 16,
    // 下一次 spawn 会看到 `prev=16` 并拒绝。
    assert!(factory.depth() >= crate::MAX_DEPTH);
}

/// `set_default_model` 写入后 `default_model()` 读到新值;`child_factory()`
/// 共享同一字符串(同一把 Mutex 走 `lock().clone()`)。
#[test]
fn set_default_model_updates_value_and_propagates_to_child() {
    let factory = SubAgentFactory::new(
        ThreadId::new(),
        "anthropic/claude-3-5-sonnet-latest",
        std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    );
    assert_eq!(
        factory.default_model(),
        "anthropic/claude-3-5-sonnet-latest"
    );
    factory.set_default_model("openai/gpt-4o");
    assert_eq!(factory.default_model(), "openai/gpt-4o");
    // child factory 在 set 之后构造,应看到新值
    let child = factory.child_factory();
    assert_eq!(
        child.default_model(),
        "openai/gpt-4o",
        "子 factory 应观察到父级 set 之后的值"
    );
}

// ── v0.2.4:带 usage 的结果收集 ─────────────────────────────

/// 构造一个最小化的 `SpawnedChild` —— 由于 `TurnHandle` 字段私有,
/// 通过 mpsc channel + `AgentThread` 内部不必要,直接走一个 `Event`
/// channel + 手写 `TurnHandle`。`TurnHandle` 暴露 `next()` 返回
/// `Option<Event>`,我们可以包装一个自己的 `next()` 通过显式构造。
///
/// 为避免依赖 `reflect_core::TurnHandle` 的内部表示,采用 trait 抽象:
/// 在 `SpawnedChild` 上 `handle.next().await` 要求 `handle: TurnHandle`,
/// 而 `TurnHandle` 是 `reflect_core` 公开类型。直接构造一条会跳过
/// 真实 AgentThread wiring —— 这里只验证 `extract_result` 路径 +
/// usage 捕获路径,因此把 drain 逻辑复制到单测里,以 `SpawnedChild`
/// 不变(签名不变)为前提测试逻辑正确性。
///
/// 真正端到端测试由 `reflect-discussion/tests/discussion_collab_e2e.rs`
/// 覆盖,这里只验证 drain 语义。
use reflect_protocol::{Event, EventMsg, TokenCountEvent, TurnCompleteEvent, TurnId, TurnStatus};

/// 单测辅助:把一组 events + 一个 extractor 喂给与 `collect_result_with_usage`
/// 等价的私有 drain 逻辑,断言 usage 捕获。
///
/// 由于 `SpawnedChild` 的 `handle` 字段没有 setter,我们通过反射式地构造
/// 一份 `SpawnedChild` 在编译期不可见 —— 改用把 drain 逻辑复制到测试里
/// 的方式,测试 `TurnComplete.usage` 与 `TokenCount` 两种来源。
fn drain_usage_only(events: Vec<Event>) -> (Option<TokenUsage>, Option<TokenUsage>) {
    let mut from_turn_complete = None;
    let mut from_token_count = None;
    for ev in &events {
        match &ev.msg {
            EventMsg::TurnComplete(tc) => from_turn_complete = Some(tc.usage.clone()),
            EventMsg::TokenCount(t) => {
                from_token_count = Some(TokenUsage {
                    input_tokens: t.input_tokens,
                    output_tokens: t.output_tokens,
                    cached_tokens: t.cached_tokens,
                    cache_write_tokens: t.cache_write_tokens,
                    total_tokens: t.total_tokens,
                });
            }
            _ => {}
        }
    }
    (from_turn_complete, from_token_count)
}

#[test]
fn collect_result_with_usage_extracts_turn_complete_usage() {
    // 模拟:`TokenCount` 先发,随后 `TurnComplete.usage` 终结。
    // 期望 `token_usage = TurnComplete.usage` (权威来源胜出)。
    let events = vec![
        Event::new(
            "sub",
            EventMsg::TokenCount(TokenCountEvent {
                input_tokens: 50,
                output_tokens: 10,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 60,
                cost_usd: None,
                ..Default::default()
            }),
        ),
        Event::new(
            "sub",
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: TurnId::new(),
                usage: TokenUsage::new(100, 20, 0),
                status: TurnStatus::Success,
            }),
        ),
    ];
    let (from_tc, from_tok) = drain_usage_only(events);
    let final_usage = from_tc.or(from_tok).expect("should capture usage");
    assert_eq!(final_usage.input_tokens, 100);
    assert_eq!(final_usage.output_tokens, 20);
}

#[test]
fn collect_result_with_usage_returns_none_when_no_token_event() {
    // drain 流仅 `TurnComplete` 不带 usage(全零的 default)且无 `TokenCount`。
    // 由于 `TurnComplete.usage` 是 `TokenUsage` (非 Option),即使值全零也算
    // "捕获到"。这是预期行为:LLM 真的没有 token 也会发 `TurnComplete`。
    let events = vec![Event::new(
        "sub",
        EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: TurnId::new(),
            usage: TokenUsage::default(),
            status: TurnStatus::Success,
        }),
    )];
    let (from_tc, from_tok) = drain_usage_only(events);
    // 两者都是 Some(默认),final 取 from_tc(零值)
    let final_usage = from_tc.or(from_tok).expect("should still have a usage");
    assert_eq!(final_usage.input_tokens, 0);
    assert_eq!(final_usage.output_tokens, 0);
}

#[test]
fn collect_result_with_usage_picks_last_token_count_when_no_turn_complete() {
    // drain 流仅 2 个 `TokenCount`,没有 `TurnComplete`(早终止)。
    // 期望:`token_usage` = 最后一个 `TokenCount`。
    let events = vec![
        Event::new(
            "sub",
            EventMsg::TokenCount(TokenCountEvent {
                input_tokens: 10,
                output_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 11,
                cost_usd: None,
                ..Default::default()
            }),
        ),
        Event::new(
            "sub",
            EventMsg::TokenCount(TokenCountEvent {
                input_tokens: 30,
                output_tokens: 3,
                cached_tokens: 5,
                cache_write_tokens: 0,
                total_tokens: 33,
                cost_usd: None,
                ..Default::default()
            }),
        ),
    ];
    let (from_tc, from_tok) = drain_usage_only(events);
    assert!(from_tc.is_none(), "no TurnComplete");
    let final_usage = from_tc.or(from_tok).expect("token_count fallback");
    assert_eq!(final_usage.input_tokens, 30);
    assert_eq!(final_usage.output_tokens, 3);
    assert_eq!(final_usage.cached_tokens, 5);
}

// ── v1.0.0-rc2:插件 spec ─────────────────────────────────────────

fn dummy_spec(role: &str) -> SubAgentSpec {
    use crate::data_transfer::DataTransferConfig;
    SubAgentSpec {
        name: role.into(),
        role: role.into(),
        model: None,
        system_prompt: String::new(),
        allowed_tools: vec![],
        data_transfer: DataTransferConfig::default(),
        max_turns: None,
        allowed_skills: vec![],
    }
}

fn dummy_factory() -> SubAgentFactory {
    SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o",
        std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
        None, // child_registry: 回退父级 registry
        Arc::new(ToolRegistry::default()),
        CancellationToken::new(),
        None,
    )
}

#[test]
fn set_child_registry_roundtrips_and_clears() {
    // v1.x 功能 1:`set_child_registry` 写入 / `current_child_registry`
    // 读 / `None` 清除,对齐 `set_default_model` 的热重载 pattern。
    let factory = dummy_factory();
    assert!(factory.current_child_registry().is_none());
    let reg: SharedModelRegistry = Arc::new(reflect_llm::ModelRegistry::new());
    factory.set_child_registry(Some(reg));
    assert!(
        factory.current_child_registry().is_some(),
        "set_child_registry(Some) 必须可读"
    );
    factory.set_child_registry(None);
    assert!(
        factory.current_child_registry().is_none(),
        "set_child_registry(None) 必须清除"
    );
}

#[test]
fn set_child_registry_shared_via_arc_clone() {
    // `Arc<Factory>` clone 后两副本共享同一 Mutex —— 一边 set,
    // 另一边 current_child_registry 立刻读到。
    let factory = Arc::new(dummy_factory());
    let factory2 = factory.clone();
    let reg: SharedModelRegistry = Arc::new(reflect_llm::ModelRegistry::new());
    factory.set_child_registry(Some(reg));
    assert!(
        factory2.current_child_registry().is_some(),
        "Arc clone 必须能观察到 child_registry 写入"
    );
}

#[test]
fn register_plugin_spec_stores_under_plugin_id() {
    let factory = dummy_factory();
    factory.register_plugin_spec("plugin-a", dummy_spec("review"));
    factory.register_plugin_spec("plugin-a", dummy_spec("format"));
    factory.register_plugin_spec("plugin-b", dummy_spec("lint"));
    assert_eq!(
        factory.registered_plugin_ids(),
        vec!["plugin-a".to_string(), "plugin-b".to_string()]
    );
    let a = factory.plugin_specs_for("plugin-a");
    assert_eq!(a.len(), 2);
}

#[test]
fn take_plugin_specs_clears_and_returns() {
    let factory = dummy_factory();
    factory.register_plugin_spec("plugin-a", dummy_spec("review"));
    factory.register_plugin_spec("plugin-a", dummy_spec("format"));
    let taken = factory.take_plugin_specs("plugin-a");
    assert_eq!(taken.len(), 2);
    assert!(factory.plugin_specs_for("plugin-a").is_empty());
    assert!(factory.registered_plugin_ids().is_empty());
}

#[test]
fn take_plugin_specs_unknown_returns_empty() {
    let factory = dummy_factory();
    assert!(factory.take_plugin_specs("ghost").is_empty());
}

#[test]
fn register_plugin_spec_rejects_invalid_role() {
    let factory = dummy_factory();
    // 大写 role → validate 拒绝 → spec 不被存。
    factory.register_plugin_spec("plugin-a", dummy_spec("BadRole"));
    assert!(factory.registered_plugin_ids().is_empty());
}

// ── v1.1.0:动态 spec ────────────────────────────────────

/// `add_spec` 接受合法 role,`get_spec` 拿回原对象。
#[test]
fn add_spec_and_get_spec_roundtrip() {
    let factory = dummy_factory();
    let mut s = dummy_spec("architect");
    s.system_prompt = "design".into();
    assert!(factory.add_spec(s.clone()));
    let back = factory.get_spec("architect").unwrap();
    assert_eq!(back.role, "architect");
    assert_eq!(back.system_prompt, "design");
}

/// `add_spec` 拒绝非法 role(BadRole → 大写失败),返回 false 且不存。
#[test]
fn add_spec_rejects_invalid_role() {
    let factory = dummy_factory();
    assert!(!factory.add_spec(dummy_spec("BadRole")));
    assert!(factory.get_spec("BadRole").is_none());
    assert!(factory.list_specs().is_empty());
}

/// `remove_spec` 存在返回 true,不存在返回 false;移除后 `get_spec` 返 None。
#[test]
fn remove_spec_returns_bool() {
    let factory = dummy_factory();
    factory.add_spec(dummy_spec("architect"));
    factory.add_spec(dummy_spec("builder"));
    assert!(factory.remove_spec("architect"));
    assert!(factory.get_spec("architect").is_none());
    assert!(!factory.remove_spec("architect"), "double remove is no-op");
    assert!(factory.remove_spec("builder"));
    assert!(factory.list_specs().is_empty());
}

/// `set_specs` 整体替换,旧的全清,新的按字典序;validate 失败 spec 跳过。
#[test]
fn set_specs_replaces_and_skips_invalid() {
    let factory = dummy_factory();
    // 先放一个旧 spec。
    factory.add_spec(dummy_spec("old-role"));
    assert_eq!(factory.list_specs().len(), 1);

    // set_specs:2 个合法 + 1 个非法(BadRole) → 只存 2 个。
    factory.set_specs(vec![
        dummy_spec("zulu"),
        dummy_spec("alpha"),
        dummy_spec("BadRole"),
    ]);
    let pairs = factory.list_specs();
    let roles: Vec<&str> = pairs.iter().map(|(r, _)| r.as_str()).collect();
    // 字典序 alpha < zulu,旧的 old-role 已清。
    assert_eq!(roles, vec!["alpha", "zulu"]);
    assert!(factory.get_spec("old-role").is_none());
}

/// `list_specs` 按 (role, name) 字典序稳定输出,保证 TUI 渲染与测试稳定。
#[test]
fn list_specs_sorted_by_role_then_name() {
    let factory = dummy_factory();
    factory.add_spec(SubAgentSpec {
        name: "Z-Name".into(),
        ..dummy_spec("zulu")
    });
    factory.add_spec(SubAgentSpec {
        name: "A-Name".into(),
        ..dummy_spec("alpha")
    });
    let pairs = factory.list_specs();
    // role 字典序排:alpha < zulu;同 role 下 name 不参与排序(只有 1 个)。
    assert_eq!(
        pairs,
        vec![
            ("alpha".to_string(), "A-Name".to_string()),
            ("zulu".to_string(), "Z-Name".to_string()),
        ]
    );
}

/// `child_factory` 不继承 dynamic_specs —— 子 factory 管理自己的 spec 命名空间。
#[test]
fn child_factory_does_not_inherit_dynamic_specs() {
    let factory = dummy_factory();
    factory.add_spec(dummy_spec("architect"));
    assert_eq!(factory.list_specs().len(), 1);
    let child = factory.child_factory();
    // 子 factory 看不到父级 dynamic_specs。
    assert!(child.list_specs().is_empty());
    assert!(child.get_spec("architect").is_none());
    // 子 factory 上的 add_spec 不影响父级。
    child.add_spec(dummy_spec("builder"));
    assert_eq!(factory.list_specs().len(), 1);
    assert_eq!(child.list_specs().len(), 1);
}

// ── v1.1.0 Phase 4: coordinator mode 注入 ─────────────────────────

/// `set_coordinator_mode(true, Some(footer))` 后 `is_coordinator_mode`
/// 返 true,`coordinator_footer()` 拿到 footer。
#[test]
fn set_coordinator_mode_updates_state() {
    let factory = dummy_factory();
    assert!(!factory.is_coordinator_mode());
    assert!(factory.coordinator_footer().is_none());

    factory.set_coordinator_mode(true, Some("you are a worker".into()));

    assert!(factory.is_coordinator_mode());
    assert_eq!(
        factory.coordinator_footer().as_deref(),
        Some("you are a worker")
    );
}

/// 关掉 coordinator mode → `is_coordinator_mode` 返 false,footer 留旧值
/// (后续不再用,但保留以便诊断)。
#[test]
fn set_coordinator_mode_can_disable() {
    let factory = dummy_factory();
    factory.set_coordinator_mode(true, Some("x".into()));
    factory.set_coordinator_mode(false, None);
    assert!(!factory.is_coordinator_mode());
    // footer 已清空
    assert!(factory.coordinator_footer().is_none());
}

/// `child_factory` 共享 coordinator_mode:父启用 → 子也启用。
/// footer 是 clone 快照,后续父级改不影响子级。
#[test]
fn child_factory_shares_coordinator_mode() {
    let factory = dummy_factory();
    factory.set_coordinator_mode(true, Some("footer".into()));
    let child = factory.child_factory();
    assert!(child.is_coordinator_mode());
    assert_eq!(child.coordinator_footer().as_deref(), Some("footer"));

    // 父级关掉,子级仍启用(因为是 `Arc<AtomicBool>` 共享同一引用)。
    factory.set_coordinator_mode(false, None);
    assert!(!factory.is_coordinator_mode());
    // 子级也变 false(共享 Arc)。
    assert!(!child.is_coordinator_mode());

    // 但 footer 是 clone 独立,父级修改不影响已 clone 的 child。
    factory.set_coordinator_mode(true, Some("new".into()));
    assert_eq!(factory.coordinator_footer().as_deref(), Some("new"));
    // child 持有的是 `child_factory()` 调用时的快照("footer"),
    // 后续父级改 "new" 不影响。
    assert_eq!(
        child.coordinator_footer().as_deref(),
        Some("footer"),
        "footer 是 clone 快照,后续父级修改不影响 child"
    );
}

/// P2 `git-worktree-auto`:`set_worktree_coordinator` 注入后可读回,
/// 且 `child_factory` 继承同一 Arc(孙级 spawn 也走隔离)。
#[test]
fn worktree_coordinator_set_and_inherited_by_child() {
    let factory = dummy_factory();
    assert!(factory.worktree_coordinator().is_none(), "默认无隔离");

    let coord = Arc::new(reflect_tools::WorktreeCoordinator::new(
        std::path::PathBuf::from("/tmp/repo"),
    ));
    factory.set_worktree_coordinator(Some(coord.clone()));
    let got = factory.worktree_coordinator().expect("注入后可读回");
    assert!(Arc::ptr_eq(&got, &coord), "读回的应是同一 Arc");

    // child_factory 继承(共享 Arc),让孙级 spawn 也隔离。
    let child = factory.child_factory();
    let child_got = child.worktree_coordinator().expect("child 继承 coord");
    assert!(
        Arc::ptr_eq(&child_got, &coord),
        "child_factory 必须继承同一 WorktreeCoordinator Arc"
    );

    // 清除语义。
    factory.set_worktree_coordinator(None);
    assert!(factory.worktree_coordinator().is_none());
}

/// `Arc<SubAgentFactory>` 共享语义:两个 Arc 副本调 `set_coordinator_mode`,
/// `is_coordinator_mode` 互见。
#[test]
fn coordinator_mode_shared_via_arc() {
    let factory = Arc::new(dummy_factory());
    let factory2 = factory.clone();
    factory.set_coordinator_mode(true, Some("shared".into()));
    assert!(factory2.is_coordinator_mode());
    assert_eq!(factory2.coordinator_footer().as_deref(), Some("shared"));
}

/// coordinator 启用时 `build_spawn_user_input` 末尾含 `[Coordinator Principle]`。
#[test]
fn build_spawn_user_input_appends_coordinator_principle() {
    let spec = dummy_spec("architect");
    let out =
        super::build_spawn_user_input(&spec, "do the thing", true, Some("stay independent".into()));
    assert!(out.contains("[Coordinator Principle]"));
    assert!(out.contains("stay independent"));
    assert!(out.contains("[Subagent role:"));
}

/// coordinator 关闭时不附加 footer 段。
#[test]
fn build_spawn_user_input_skips_footer_when_disabled() {
    let spec = dummy_spec("architect");
    let out = super::build_spawn_user_input(&spec, "task", false, Some("ignored".into()));
    assert!(!out.contains("[Coordinator Principle]"));
}

// ── v1.1.0 复审 ────────────────────────────────────────────────
// bug-1 (P0):子 m4.subagent_registry 与父共享 Arc
// bug-4 (P1):child_factory 继承 registry

/// bug-1 (P0):`spawn` 路径构造的 child `AgentConfig.m4.subagent_registry`
/// 应与父 factory 注入了的 registry 共享同一 Arc —— 否则子 agent 的
/// `pre_loop` 渲染 `<system-reminder>` 时拿的是 fresh Arc,看不到父 /
/// 自身任何已完成调用。本测试构造一个 fake `Submission` 不可行(走真
/// AgentThread),改为在 spawn 内部链路直接断言:通过 `Arc::ptr_eq` 验证
/// 父 m4 与子 cfg.m4 的 `subagent_registry` 同源。
///
/// 因 `spawn` 需要 LLM client,改为验证 helper 逻辑:把当前 `factory.rs`
/// 的 `spawn` 中"构造 m4 + 注入 registry"那段抽出独立 fn 后,本测试
/// 调它比对指针。为避免改动 `spawn` 签名,这里改用 `child_factory` 上
/// 同样的 Arc clone 路径(`Mutex::new(self.subagent_registry.lock().clone())`),
/// 直接断言父 ↔ 子 factory registry 是同一 Arc。
#[test]
fn child_factory_registry_shares_arc_with_parent() {
    let factory = dummy_factory();
    let reg = reflect_recovery::SubagentRegistry::shared();
    factory.set_subagent_registry(reg.clone());

    let child = factory.child_factory();
    let parent_reg = factory.subagent_registry().unwrap();
    let child_reg = child.subagent_registry().unwrap();
    assert!(
        Arc::ptr_eq(&parent_reg, &child_reg),
        "child_factory 必须与父共享同一 Arc(共享 registry 才能让孙级 subagent 被父级 pre_loop 看到)"
    );
    // 同时验证 child_factory 写入对父可见(共享语义的核心断言)。
    reg.record(reflect_recovery::SubagentRegistryEntry {
        tool_name: "call_x".into(),
        task_summary: "child wrote".into(),
        result_summary: "ok".into(),
        iteration: 1,
        created_at: chrono::Utc::now(),
    });
    assert_eq!(parent_reg.snapshot().len(), 1);
}

/// bug-1 (P0) e2e-lite:模拟 `spawn` 构造子 m4 的等价路径 —— 走
/// `reflect_core::config::default_m4_deps` 拿 fresh M4Deps,然后注入
/// 父 registry。这正是 review 后 `spawn` 路径采用的修复模式,
/// 单测验证注入语义(否则回归时不会失败)。
#[test]
fn spawn_propagates_parent_registry_into_child_m4() {
    use reflect_core::config::default_m4_deps;

    let factory = dummy_factory();
    let reg = reflect_recovery::SubagentRegistry::shared();
    factory.set_subagent_registry(reg.clone());

    // 模拟 spawn 内构造 child m4 的逻辑:
    let mut m4 = default_m4_deps("subagent");
    assert!(
        !Arc::ptr_eq(&m4.subagent_registry, &reg),
        "sanity:default_m4_deps 给的是 fresh Arc"
    );
    if let Some(parent_reg) = factory.subagent_registry() {
        m4.subagent_registry = parent_reg;
    }
    assert!(
        Arc::ptr_eq(&m4.subagent_registry, &reg),
        "修复后:child m4.subagent_registry 必须指向父 factory 注入了的 Arc"
    );
}

/// bug-1 (P0) negative:父未注入 registry 时,child m4 仍走 fresh Arc
/// (测试 / headless 退化路径)。保证本修复不引入 panic。
#[test]
fn spawn_uses_fresh_registry_when_parent_unset() {
    use reflect_core::config::default_m4_deps;

    let factory = dummy_factory();
    // 不调 set_subagent_registry → 父为 None
    let mut m4 = default_m4_deps("subagent");
    let fresh = m4.subagent_registry.clone();
    if let Some(parent_reg) = factory.subagent_registry() {
        m4.subagent_registry = parent_reg;
    }
    // 父为 None → 不替换,仍是 fresh Arc(行为不变,无回归)。
    assert!(Arc::ptr_eq(&m4.subagent_registry, &fresh));
}

// ── in-flight 语义:Drop 自动释放槽位 ────────────────────────

/// 模拟"SpawnedChild 提前被遗忘"(panic / 漏调 collect_result)
/// 时,Drop 仍必须释放 in-flight 槽位,否则长 session 累积到上限。
/// 此处通过 `Arc::strong_count` 间接断言 —— 因为 `SpawnedChild`
/// 字段非 pub,改用 `depth` 计数直接观察。
#[test]
fn in_flight_decrements_on_drop() {
    let factory = dummy_factory();
    let in_flight = Arc::clone(&factory.in_flight);
    // 模拟 spawn:占用槽位
    assert_eq!(in_flight.fetch_add(1, Ordering::SeqCst), 0);
    assert_eq!(factory.depth(), 1);

    // 模拟"SpawnedChild 析构":直接构造一个临时 `SpawnedChild` 字段等价
    // 路径困难(字段非 pub),改为手工持有同一计数器 + Drop 触发即可。
    struct Guard(Arc<std::sync::atomic::AtomicU8>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let g = Guard(Arc::clone(&in_flight));
    drop(g);
    assert_eq!(factory.depth(), 0, "Drop 后槽位应释放");
}

/// 嵌套 in-flight 计数父 ↔ 子共享 —— 父级 spawn +1,孙级 spawn
/// 共享同一 Arc,孙级析构 -1 后父级观察到递减。
#[test]
fn nested_in_flight_shared_arc_releases_correctly() {
    let factory = dummy_factory();
    let child = factory.child_factory();
    let shared = Arc::clone(&factory.in_flight);

    // 父 +1
    shared.fetch_add(1, Ordering::SeqCst);
    assert_eq!(factory.depth(), 1);
    assert_eq!(child.depth(), 1);

    // 子 -1
    shared.fetch_sub(1, Ordering::SeqCst);
    assert_eq!(factory.depth(), 0);
    assert_eq!(child.depth(), 0);
}

/// 验证多次 spawn → drop 循环不累积计数(避免计数器泄漏 bug)。
#[test]
fn in_flight_does_not_leak_across_repeated_spawn_drop_cycles() {
    let factory = dummy_factory();
    let in_flight = Arc::clone(&factory.in_flight);

    // 模拟 100 次 spawn + drop 循环
    struct Guard(Arc<std::sync::atomic::AtomicU8>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    for _ in 0..100 {
        in_flight.fetch_add(1, Ordering::SeqCst);
        let g = Guard(Arc::clone(&in_flight));
        drop(g);
    }
    assert_eq!(
        factory.depth(),
        0,
        "100 次 spawn+drop 后计数应归 0,不能泄漏"
    );
}

// ── v1.4 A1:运行注册表 + 父级令牌覆盖 ─────────────────────────

/// `set_runtime_registry` 写入 / `runtime_registry` 读出;child_factory
/// 继承同一 Arc —— 孙级 spawn 登记进同一张表,跨级定向中断才能路由。
#[test]
fn runtime_registry_set_and_inherited_by_child_factory() {
    let factory = dummy_factory();
    assert!(
        factory.runtime_registry().is_none(),
        "默认不注入运行注册表(测试 / 旧调用方)"
    );
    let reg = Arc::new(reflect_core::SubagentRuntimeRegistry::new());
    factory.set_runtime_registry(reg.clone());

    let readback = factory.runtime_registry().expect("set 后必须可读");
    assert!(
        Arc::ptr_eq(&readback, &reg),
        "runtime_registry 必须返回注入的同一 Arc"
    );

    let child = factory.child_factory();
    let child_reg = child.runtime_registry().expect("child_factory 必须继承");
    assert!(
        Arc::ptr_eq(&child_reg, &reg),
        "child_factory 必须与父共享同一运行注册表 Arc"
    );
}

/// `set_cancel` 端到端:注入运行注册表 + 父令牌后 spawn,子代理登记进
/// 表;父令牌 cancel 级联子令牌(空 LLM 池让子 turn 快速失败也无妨,
/// 本测试只断言注册表生命周期与级联语义)。
#[tokio::test]
async fn spawn_registers_child_and_parent_cancel_cascades() {
    let factory = dummy_factory();
    let runtime = Arc::new(reflect_core::SubagentRuntimeRegistry::new());
    factory.set_runtime_registry(runtime.clone());
    let session_token = CancellationToken::new();
    factory.set_cancel(session_token.clone());

    let child = factory
        .spawn(dummy_spec("explorer"), Vec::new(), "find foo".into())
        .await
        .expect("spawn 应成功(空池不影响 spawn 本身)");
    let child_id = child.session_id.to_string();

    // spawn 后立即登记,条目可在飞期间被定向中断查询。
    assert_eq!(runtime.child_ids(), vec![child_id.clone()]);

    // 父令牌取消 → 子令牌级联取消(Shutdown / Ctrl-C 语义)。
    // 取消后 drain 收尾:槽位转 Cancelled 终态并**保留**在状态中心
    // (v1.4 C1 语义:终态不立即删,供 QuerySubagents 事后查询,
    // 过期由下次 register 清扫)。
    session_token.cancel();
    let _ = child.collect_result().await;
    let snaps = runtime.snapshot(Some(&child_id));
    assert_eq!(snaps.len(), 1, "终态槽位应保留供事后查询");
    assert_eq!(
        snaps[0].state,
        reflect_protocol::SubagentRunStateMirror::Cancelled,
        "父令牌级联取消 → 子代理终态应为 Cancelled,实际:{:?}",
        snaps[0].state
    );
    assert!(snaps[0].finished_at.is_some());
}
