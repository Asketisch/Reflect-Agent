//! 门面 `ReflectBuilder::build_async` 的插件挂载集成测试。
//!
//! 在隔离 HOME 中真实安装示例插件 `examples/plugin-demo`,断言
//! `build_async` 后 `Reflect::plugin_runtime()` 的命令注册表已含
//! `demo:hello`(skills / agents / hooks / MCP 共用同一挂载路径)。
//!
//! HOME / env 改写用 tokio Mutex 串行化(锁横跨 await 点)。

use std::sync::Arc;

static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn build_async_mounts_demo_plugin_commands() {
    let _lock = ENV_MUTEX.lock().await;
    let orig_home = std::env::var("HOME").ok();
    let orig_key = std::env::var("OPENAI_API_KEY").ok();
    let home = tempfile::tempdir().expect("tempdir");
    let ws = tempfile::tempdir().expect("tempdir");
    // SAFETY: ENV_MUTEX 已串行化。
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("OPENAI_API_KEY", "test-key");
    }

    let result = mount_and_lookup(home.path(), ws.path()).await;

    // SAFETY: 同上,结束前还原 env。
    unsafe {
        match orig_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match orig_key {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }
    let runtime = result.expect("build_async 应成功挂载插件");
    let guard = runtime.lock().await;
    let rt = guard.as_ref().expect("HOME 已隔离,PluginRuntime 应存在");
    let hello = rt
        .commands()
        .lookup("demo:hello")
        .expect("demo:hello 命令应已挂载");
    // 命令正文可展开(指向 cache 中的 md 文件)。
    let body = reflect_plugin::expand_command(&hello, "世界").expect("命令正文应可读");
    assert!(body.contains("打一声招呼"), "unexpected body: {body}");
}

async fn mount_and_lookup(
    home: &std::path::Path,
    ws: &std::path::Path,
) -> anyhow::Result<reflect_plugin::SharedPluginRuntime> {
    // 真实安装示例插件(复制进隔离 HOME 的 plugins cache)。
    let demo =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../examples/plugin-demo");
    let plugins_root = home.join(".reflect").join("plugins");
    let mut manager = reflect_plugin::PluginManager::load(&plugins_root)?;
    manager.install_local(
        &demo,
        &reflect_plugin::MarketplaceName::inline(),
        reflect_plugin::PluginScope::User,
    )?;

    let agent = reflect::Reflect::builder("openai/gpt-4o")
        .workspace(ws)
        .with_plugins(Some(vec!["demo@inline".to_string()]))
        .build_async()
        .await?;
    // plugin_runtime() 返回共享句柄;clone 出绕过生命周期。
    let runtime: Arc<tokio::sync::Mutex<Option<reflect_plugin::PluginRuntime>>> =
        agent.plugin_runtime().clone();
    Ok(runtime)
}
