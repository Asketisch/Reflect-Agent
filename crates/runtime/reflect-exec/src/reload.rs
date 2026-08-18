//! Config 热重载:registry 替换、section diff、事件推送。
//!
//! 从原 `lib.rs` 原样抽出。这些自由函数构成单一职责的聚合点,
//! 既被 `async_main` 的 reload 任务调用,也被集成/单元测试覆盖。

use std::sync::Arc;

use reflect_config::ConfigWatcher;
use reflect_core::AgentConfig;
use reflect_llm::ModelRegistry;
use reflect_mcp::{McpConnectionManager, McpServerConfig};
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;

use crate::apply_coordinator_from_config;
use crate::bootstrap_plugins;

/// 监听 config watcher,每次变更重建 `ModelRegistry` 内的 provider client。
/// 已发出的 LLM 请求不受影响;下一个 turn 用新 client。
///
/// M8 P1b:除 registry 替换外,还会推一条 `EventMsg::ConfigReloaded`
/// 到 `event_tx`,让 headless `reflect exec` 的 JSONL 流与 TUI 都能
/// 感知到本次 reload。`path` 是解析后的 config 文件路径,会带入
/// 事件 payload。
///
/// v0.2.2: 新增 `previous_cfg` / `agent_cfg` / `factory` / `recorder` 参数。
/// `previous_cfg` 用于和 `new_cfg` 做 `diff_sections`;`agent_cfg` 与
/// `factory` 用于把新的 model spec 推到 agent 线程与后续 spawn 的子 agent;
/// `recorder` 在 provider 真正变更时写第二次 `RolloutRecord::SessionMeta`,
/// 让热重载对 JSONL 也是可见的。Resume 分支没 factory,传 `None`。
/// 9 个参数超过 clippy 默认 7 上限 —— 同 `handle_reload`:这是单一职责
/// 纯路径聚合点,bundle 进 struct 增加跳转成本而无功能收益。
#[allow(clippy::too_many_arguments)]
pub fn spawn_reload_task(
    registry: Arc<ModelRegistry>,
    watcher: ConfigWatcher,
    event_tx: tokio::sync::mpsc::Sender<reflect_protocol::Event>,
    path: std::path::PathBuf,
    previous_cfg: reflect_config::ReflectConfig,
    agent_cfg: AgentConfig,
    factory: Option<Arc<SubAgentFactory>>,
    recorder: Option<Arc<dyn reflect_protocol::RolloutRecorder>>,
    mcp_manager: Option<Arc<McpConnectionManager>>,
    tools: Arc<ToolRegistry>,
    plugin_runtime: Option<bootstrap_plugins::SharedPluginRuntime>,
) {
    tokio::spawn(async move {
        let mut rx = watcher.subscribe();
        // 跳过初始值(启动期间已应用)。
        if rx.changed().await.is_err() {
            return;
        }
        // 在 reload 循环外持有 mutable 状态,每次迭代更新为最新 cfg。
        let mut prev = previous_cfg;
        while rx.changed().await.is_ok() {
            let new_cfg = rx.borrow().clone();
            match handle_reload(
                &prev,
                &new_cfg,
                &registry,
                &agent_cfg,
                factory.as_deref(),
                recorder.as_deref(),
                &path,
                &event_tx,
                mcp_manager.as_deref(),
                &tools,
                plugin_runtime.as_ref(),
            )
            .await
            {
                Ok(()) => tracing::debug!("config reload handled; event pushed"),
                Err(e) => tracing::warn!(error = %e, "config reload emit failed"),
            }
            prev = new_cfg;
        }
        tracing::debug!("config reload task exiting");
    });
}

/// M8 P1b + v0.2.2:纯(好吧,async)reload handler。
///
/// 流程:
/// 1. `apply_to_registry` 替换 provider `Arc<dyn ModelClient>`(M7)。
/// 2. `diff_sections(old, new)` 算出本轮变更的 section 列表。
/// 3. 若 `resolved_model_spec` 在 old/new 之间变了,调 `agent_cfg.set_model(...)`
///    与 `factory.set_default_model(...)` 同步推给运行中的 agent 和后续 spawn 的
///    子 agent;再 emit 一次 `SessionConfigured` + 写第二次 `RolloutRecord::SessionMeta`,
///    让 JSONL / TUI banner 都看到新 model。
/// 4. 推 `EventMsg::ConfigReloaded { sections_changed, .. }`。
///
/// registry apply 失败时**不**推送该事件
/// (M7 行为:保留旧 provider,仅 `tracing::warn!`)。
///
/// 抽取为 `pub` 自由函数,便于下方 `tests::` 中的单元测试覆盖事件构造
/// 路径,无需触碰 `notify` 或真实的 `ConfigWatcher`。
///
/// 10 个参数超过 clippy 默认 7 上限 —— 这是有意为之,本函数是单一职责
/// 的纯路径聚合点,bundle 参数进 struct 会增加跳转成本而无功能收益。
#[allow(clippy::too_many_arguments)]
pub async fn handle_reload(
    old_cfg: &reflect_config::ReflectConfig,
    new_cfg: &reflect_config::ReflectConfig,
    registry: &Arc<ModelRegistry>,
    agent_cfg: &AgentConfig,
    factory: Option<&SubAgentFactory>,
    recorder: Option<&dyn reflect_protocol::RolloutRecorder>,
    path: &std::path::Path,
    event_tx: &tokio::sync::mpsc::Sender<reflect_protocol::Event>,
    mcp_manager: Option<&McpConnectionManager>,
    tools: &ToolRegistry,
    plugin_runtime: Option<&bootstrap_plugins::SharedPluginRuntime>,
) -> Result<(), String> {
    if let Err(e) = new_cfg.apply_to_registry(registry) {
        return Err(format!("apply_to_registry failed: {e}"));
    }
    let sections_changed = diff_sections(old_cfg, new_cfg);
    let old_spec = old_cfg.resolved_model_spec();
    let new_spec = new_cfg.resolved_model_spec();
    let model_changed = old_spec != new_spec && new_spec.is_some();
    if let Some(new_model) = new_spec.as_ref() {
        if model_changed {
            agent_cfg.set_model(new_model.clone());
            if let Some(f) = factory {
                f.set_default_model(new_model.clone());
            }
            tracing::info!(
                old = old_spec.as_deref().unwrap_or("<none>"),
                new = %new_model,
                "config reload: model swapped"
            );
            // provider/model 真的变了 → 重新 emit SessionConfigured 让
            // TUI banner 刷新。
            //
            // v1.x:不再向 JSONL 写第二条 `SessionMeta`。此前每次热重载切
            // model 都 append 一条带**新 ThreadId::new()** 的 SessionMeta,
            // 导致同一 JSONL 文件出现多个 session id —— `list_sessions` 按
            // 首行 SessionMeta 的 id 归类,后续会话与文件名漂移,`/session`
            // 列表里一次对话变多条。SessionConfigured 事件足以刷新 banner,
            // JSONL 不需要重复 header(rotation 机制已保证多文件场景的连续性)。
            let _ = recorder; // 参数保留(签名稳定),仅不再写重复 header。
            let sc = reflect_protocol::SessionConfiguredEvent::new(
                new_model.clone(),
                provider_of_spec(new_model),
            );
            event_tx
                .send(reflect_protocol::Event::new(
                    reflect_protocol::EVENT_ID_NONE,
                    reflect_protocol::EventMsg::SessionConfigured(sc),
                ))
                .await
                .map_err(|e| format!("event_tx closed (SessionConfigured): {e}"))?;
        }
    }
    let event = reflect_protocol::Event::new(
        reflect_protocol::EVENT_ID_NONE,
        reflect_protocol::EventMsg::ConfigReloaded(reflect_protocol::ConfigReloadedEvent {
            path: path.to_path_buf(),
            sections_changed: sections_changed.clone(),
            at: std::time::SystemTime::now(),
        }),
    );

    // v0.3: MCP server diff。校验失败不阻塞 ConfigReloaded 推送,
    // 只 warn + 跳过 reload —— 现有 server 继续运行,等下一次保存若
    // 配置仍有错用户能持续看到。
    if old_cfg.mcp_servers != new_cfg.mcp_servers {
        if let Some(manager) = mcp_manager {
            match new_cfg.mcp_server_configs() {
                Ok(new_shapes) => {
                    let new_configs: Vec<McpServerConfig> =
                        new_shapes.into_iter().map(McpServerConfig::from).collect();
                    manager
                        .reload(new_configs, |_name, tool_names| {
                            for tn in tool_names {
                                tools.unregister(tn);
                            }
                        })
                        .await
                        .map_err(|e| format!("mcp reload: {e}"))?;
                }
                Err(e) => {
                    tracing::warn!(error = %e,
                        "MCP config invalid on reload; existing servers kept running");
                }
            }
        }
    }

    // v1.1.0 Phase 4:`[coordinator]` 段变更 → 热重载 coordinator 开关、
    // footer、WriteNote/ReadNotes 工具与 prompt_builder section。
    if sections_changed.iter().any(|s| s == "coordinator") {
        if let Some(f) = factory {
            apply_coordinator_from_config(
                new_cfg,
                agent_cfg.current_workspace().as_path(),
                agent_cfg.m4.as_ref(),
                f,
                tools,
            );
        }
    }

    // v1.0.0-rc2: `[plugins]` 段变更 → 同步 enabled_plugins 挂载。
    if sections_changed.iter().any(|s| s == "plugins") {
        if let Some(rt) = plugin_runtime {
            bootstrap_plugins::reload_plugins(rt, &new_cfg.plugins.enabled_plugins).await;
        }
    }

    // v1.2 P1-12:`[token_budget]` 段变更 → 更新会话预算上限。env
    // `REFLECT_TOKEN_BUDGET` 优先于 TOML;`None` = 关闭预算(仅靠
    // max_iterations)。下一轮 `model_call` 读最新值。
    if sections_changed.iter().any(|s| s == "token_budget") {
        let budget = reflect_core::config::token_budget_from_env(
            new_cfg
                .token_budget
                .as_ref()
                .and_then(|s| s.session_total_tokens),
        );
        agent_cfg.set_token_budget(budget);
        tracing::info!(?budget, "config reload: token_budget updated");
    }

    // v1.x:`active.max_iterations` 段变更 → 更新全局迭代上限。env
    // `REFLECT_MAX_ITERATIONS` 优先于 TOML;缺省回退默认 32。下一轮
    // `submission_loop` 构造 `NodeContext` 时读最新值。
    if sections_changed.iter().any(|s| s == "active") {
        let max_iter = reflect_core::config::max_iterations_from_env(new_cfg.active.max_iterations);
        agent_cfg.set_max_iterations(max_iter);
        tracing::info!(max_iter, "config reload: active.max_iterations updated");
    }

    // v1.x 功能 1:`[subagent_providers]` 段变更 → 重建 child registry。
    // 后续 `spawn()` 拿新 registry(独立 base_url + api_key + model)。
    // `None` = 清除独立凭证,回退父子共享(向后兼容)。
    if sections_changed.iter().any(|s| s == "subagent_providers") {
        if let Some(f) = factory {
            let child_reg = new_cfg.to_child_registry().map(Arc::new);
            f.set_child_registry(child_reg.clone());
            tracing::info!(
                has_child_registry = child_reg.is_some(),
                "config reload: subagent_providers updated"
            );
        }
    }

    event_tx
        .send(event)
        .await
        .map_err(|e| format!("event_tx closed: {e}"))
}

/// 比较两份 `ReflectConfig`,返回发生变化的 section 名列表。
/// v0.2.2 升级替代 M8 的 `vec!["all"]` 占位实现。
///
/// v0.3: 加 `mcp_servers` —— 走 `PartialEq` 直接比对,manager 用
/// `entry_unchanged` 做字段级细分(只重启真改了 command/args/env/url
/// 的 server,timeout 变化不 restart)。
pub fn diff_sections_for_test(
    old: &reflect_config::ReflectConfig,
    new: &reflect_config::ReflectConfig,
) -> Vec<String> {
    diff_sections(old, new)
}

pub(crate) fn diff_sections(
    old: &reflect_config::ReflectConfig,
    new: &reflect_config::ReflectConfig,
) -> Vec<String> {
    let mut out = Vec::new();
    if old.active != new.active {
        out.push("active".to_string());
    }
    if old.anthropic != new.anthropic {
        out.push("anthropic".to_string());
    }
    if old.openai != new.openai {
        out.push("openai".to_string());
    }
    if old.ollama != new.ollama {
        out.push("ollama".to_string());
    }
    if old.compact != new.compact {
        out.push("compact".to_string());
    }
    // v1.2 P1-12:`[token_budget]` 段变更 → reload 时重新生效(下一轮
    // model_call 读最新 budget)。预算变更不影响已计入的 session_usage。
    if old.token_budget != new.token_budget {
        out.push("token_budget".to_string());
    }
    if old.hooks != new.hooks {
        out.push("hooks".to_string());
    }
    if old.mcp_servers != new.mcp_servers {
        out.push("mcp_servers".to_string());
    }
    // v1.0.0-rc2: `[plugins]` 段变更 → PluginManager 走 sync 路径
    // (enable 新加的、disable 移除的、reload 修改的)。
    if old.plugins != new.plugins {
        out.push("plugins".to_string());
    }
    // v1.1.0 Phase 4: `[coordinator]` 段变更 → reload 时 coordinator
    // 重新生效(enabled / system_prompt_path / max_workers)。
    if old.coordinator != new.coordinator {
        out.push("coordinator".to_string());
    }
    // v1.2 P1: `[telemetry]` 段变更 → reload 时触发 writer 重开文件
    // (用户改了 dir / max_bytes / enabled)。
    if old.telemetry != new.telemetry {
        out.push("telemetry".to_string());
    }
    // v1.2 P1: `[goal]` 段变更 → reload 时重新生效(max_turns / 校验模型)。
    if old.goal != new.goal {
        out.push("goal".to_string());
    }
    // v1.x 功能 2:`[[subagents]]` 段变更 → reload 时重建 spec 注册
    // (用户增删改 subagent 定义)。`Vec<SubagentSpecConfig>` 走 PartialEq。
    if old.subagents != new.subagents {
        out.push("subagents".to_string());
    }
    // `[web_search]` 段变更 → reload 报告(实际 key 注入发生在 bootstrap,
    // 热重载后下一进程实例生效;此处仅做 diff 通知)。
    if old.web_search != new.web_search {
        out.push("web_search".to_string());
    }
    out
}

/// 把 `"{provider}/{model}"` 拆分,返回 provider 段。
/// 与 `reflect_core::submission_loop::provider_of` 同语义 —— 这里是
/// SessionConfigured 重新 emit 时的局部副本,避免 `submission_loop` 私有
/// 工具跨 crate 暴露。
fn provider_of_spec(model_spec: &str) -> String {
    model_spec
        .split_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
