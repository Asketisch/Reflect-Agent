# Reflect Examples

`reflect` crate 提供 5 个可运行示例,演示公共库门面。所有示例都基于
M6 引入的 `Reflect::builder()` + `EventStream` 新 API。

| 示例 | 说明 | 运行 |
|---------|---------------|-----|
| `headless_run.rs`     | 最小可用配置:一条 prompt 跑到 `TurnComplete`。 | `cargo run -p reflect --example headless_run -- "what is 2+2?"` |
| `custom_tool.rs`      | 实现 `Tool` trait(`EchoTool`)并注册到 thread 的 `ToolRegistry`。 | `cargo run -p reflect --example custom_tool` |
| `multi_turn.rs`       | 同一 `Reflect` 上的 3 条顺序 turn;历史在 turn 间保持。 | `cargo run -p reflect --example multi_turn` |
| `custom_provider.rs`  | 注册第三个 `ModelClient`(`MockLlmClient`)让 agent 离线运行。 | `cargo run -p reflect --example custom_provider` |
| `hook_listener.rs`    | 注册两个 hook:`TokenUsageHook`(计数器)与 `DangerousCommandHook`(拒绝 `rm -rf /`)。 | `cargo run -p reflect --example hook_listener` |

## 环境变量

- `OPENAI_API_KEY` —— 启用 OpenAI provider。
- `ANTHROPIC_API_KEY` —— 启用 Anthropic provider。
- `REFLECT_MODEL` —— 覆盖默认模型规格(`openai/gpt-4o`)。

多数示例设置 `REFLECT_MODEL=mock`(或默认就是它)以在无 API key 时离线运行。
`headless_run` 需要真实 key。

## 通用模式

每个示例都遵循同样的形状:

```rust
use reflect::{Reflect, Submission};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Reflect::builder("openai/gpt-4o").build()?;
    let mut stream = agent.submit(Submission::user_input("...")).await;
    while let Some(event) = stream.next().await {
        match event.msg {
            reflect::EventMsg::AgentMessageDelta(d) => print!("{}", d.delta),
            reflect::EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    Ok(())
}
```

门面完整映射见 `docs/library-api.md`(M6.3 后续工作)。
