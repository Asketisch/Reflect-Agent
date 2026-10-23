//! 插件链路端到端集成测试(离线,mock 模型)。
//!
//! 全链路:示例插件 `examples/plugin-demo` 在隔离 HOME 中真实
//! `install_local` → `PluginRuntime` 挂载五类 capability →
//! `serve_session` 内提交 `/demo:hello 张三` → 断言 mock 模型收到的
//! 是命令 md 展开后的正文,而非原始 `/demo:hello` 文本。
//!
//! HOME 隔离:`PluginRuntime` 与 rollout 目录都按 `$HOME` 解析,测试
//! 期间改写 HOME 并用 tokio Mutex 串行化(锁要横跨 await 点)。

use std::sync::Arc;
use std::time::Duration;

use reflect_core::{AgentConfig, AgentThread};
use reflect_exec::serve::serve_session;
use reflect_hooks::HookEngine;
use reflect_llm::{ContentBlock, CredentialPool, MockClient, MockReply, ModelRegistry, PoolEntry};
use reflect_mcp::McpConnectionManager;
use reflect_plugin::{
    MarketplaceName, PluginId, PluginManager, PluginScope, bootstrap_plugins, reload_plugins,
};
use reflect_protocol::{Event, EventMsg, Op, Submission, ThreadId};
use reflect_skills::SkillsCatalog;
use reflect_subagent::SubAgentFactory;
use reflect_tools::ToolRegistry;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 仓库内示例插件(`crates/runtime/reflect-exec` → 仓库根 3 级向上)。
const DEMO_PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../examples/plugin-demo");

/// 改写 `$HOME` 的互斥锁(锁横跨 await,用 tokio Mutex 避免
/// `clippy::await_holding_lock`)。
static HOME_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 构造注册了脚本化 mock provider 的 registry,并返回 client 句柄供
/// 断言「模型实际收到的请求」。
fn mock_registry_with_client(script: Vec<MockReply>) -> (Arc<ModelRegistry>, Arc<MockClient>) {
    let client = Arc::new(MockClient::with_script(script));
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "mock",
        CredentialPool {
            entries: vec![PoolEntry {
                client: client.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    (registry, client)
}

fn user_text_of_last_request(client: &MockClient) -> String {
    let requests = client.recorded_requests();
    let last = requests.last().expect("mock 模型应至少收到一次请求");
    last.messages
        .iter()
        .rev()
        .find_map(|m| match m {
            reflect_llm::ChatMessage::User(uc) => Some(
                uc.blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// 从事件流里等 turn 收尾事件(超时 panic)。
async fn expect_turn_done(rx: &mut mpsc::Receiver<Event>) -> Event {
    let deadline = Duration::from_secs(30);
    loop {
        let ev = tokio::time::timeout(deadline, rx.recv())
            .await
            .expect("等待事件超时")
            .expect("事件通道提前关闭");
        if matches!(
            ev.msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::Error(_)
        ) {
            return ev;
        }
    }
}

#[tokio::test]
async fn plugin_command_expansion_reaches_model_expanded() {
    let _lock = HOME_MUTEX.lock().await;
    let orig_home = std::env::var("HOME").ok();
    let home = tempfile::tempdir().expect("tempdir");
    // SAFETY: HOME_MUTEX 已串行化,本测试二进制内无并发读者。
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    let result = run_plugin_expansion_scenario().await;
    // SAFETY: 同上,结束时还原 HOME。
    unsafe {
        match orig_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
    result.expect("插件命令展开场景应成功");
}

async fn run_plugin_expansion_scenario() -> anyhow::Result<()> {
    // 1. 真实安装示例插件到隔离 HOME 的 plugins cache。
    let plugins_root = home_path().join(".reflect").join("plugins");
    let mut manager = PluginManager::load(&plugins_root)?;
    let (plugin_id, _entry) =
        manager.install_local(DEMO_PLUGIN, &MarketplaceName::inline(), PluginScope::User)?;
    assert_eq!(plugin_id, PluginId::inline("demo").unwrap());

    // 2. 构造五类共享 registry + 运行时挂载。
    let tools = Arc::new(ToolRegistry::default());
    let hooks = Arc::new(HookEngine::new());
    let (mcp_tx, _mcp_rx) = mpsc::channel::<reflect_mcp::McpLifecycleEvent>(16);
    let mcp = Arc::new(McpConnectionManager::new(mcp_tx));
    let skills = Arc::new(SkillsCatalog::new());
    let (model_registry, client) = mock_registry_with_client(vec![MockReply::Text {
        text: "收到".into(),
    }]);
    let factory = Arc::new(SubAgentFactory::new(
        ThreadId::new(),
        "mock/mock-1",
        model_registry.clone(),
        None,
        tools.clone(),
        CancellationToken::new(),
        None,
    ));
    let plugin_runtime = bootstrap_plugins(
        tools.clone(),
        hooks,
        mcp,
        skills,
        factory,
        // 显式启用列表(等价 config.toml [plugins].enabled_plugins;
        // 条目必须为 `name@marketplace` 全名,与 CLI enable 写入的一致)。
        &["demo@inline".to_string()],
        None,
    )
    .await;
    // 挂载必须真的发生了:命令注册表里有 demo:hello。
    {
        let guard = plugin_runtime.lock().await;
        let rt = guard.as_ref().expect("HOME 已隔离,PluginRuntime 应存在");
        assert!(rt.commands().lookup("demo:hello").is_some());
    }

    // 3. mock 模型 thread + serve 会话。
    let ws = tempfile::tempdir()?;
    let thread = Arc::new(AgentThread::new(
        AgentConfig::new("mock/mock-1", ws.path()),
        model_registry,
        tools.clone(),
        None,
        None,
    ));
    let (mut client_w, server_r) = tokio::io::duplex(8192);
    let (sink, mut sink_rx) = mpsc::channel::<Event>(256);
    let serve_task = tokio::spawn(serve_session(
        thread,
        tools,
        plugin_runtime.clone(),
        server_r,
        sink,
    ));

    // 4. 提交 slash 命令形式的用户输入。
    let sub = Submission::with_id(
        "t1",
        Op::UserInput {
            items: vec![reflect_protocol::UserInputItem::Text {
                text: "/demo:hello 张三".into(),
            }],
            thread_settings: Default::default(),
        },
    );
    let mut line = serde_json::to_string(&sub)?;
    line.push('\n');
    client_w.write_all(line.as_bytes()).await?;
    client_w.flush().await?;

    // 5. 等 turn 收尾,断言模型收到的是展开后的正文。
    let done = expect_turn_done(&mut sink_rx).await;
    assert!(
        !matches!(done.msg, EventMsg::Error(_)),
        "turn 不应报错: {done:?}"
    );
    let user_text = user_text_of_last_request(&client);
    assert!(
        user_text.contains("请用一句中文向 **张三**"),
        "模型应收到展开后的命令正文,实际: {user_text:?}"
    );
    assert!(
        !user_text.trim_start().starts_with("/demo:hello"),
        "不应把原始命令输入透传给模型,实际: {user_text:?}"
    );

    // 6. 收尾:清空 enabled 列表触发反注册(停掉插件 MCP 子进程),再
    //    Shutdown 退出 serve。
    reload_plugins(&plugin_runtime, &[]).await;
    client_w
        .write_all(b"{\"id\":\"bye\",\"op\":{\"type\":\"shutdown\"}}\n")
        .await?;
    client_w.flush().await?;
    let _ = tokio::time::timeout(Duration::from_secs(10), serve_task).await;
    Ok(())
}

fn home_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").expect("HOME 已在测试内设置"))
}
