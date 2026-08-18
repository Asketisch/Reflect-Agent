//! `headless_run` — Reflect 库的最小可运行示例。
//!
//! 从环境变量读取 API key,经 `Reflect` 门面构造一个 `AgentThread`,
//! 提交一条 prompt,并把事件流输出到 stdout。除非设置 `REFLECT_MODEL`,
//! 否则使用默认 `openai/gpt-4o`。
//!
//! 运行:
//! ```bash
//! OPENAI_API_KEY=sk-... cargo run -p reflect --example headless_run -- "what is 2+2?"
//! ```

use reflect::{Reflect, Submission};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let prompt = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: headless_run <prompt>"))?;
    let model = std::env::var("REFLECT_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let agent = Reflect::builder(model).build()?;
    let mut stream = agent.submit(Submission::user_input(&prompt)).await;

    while let Some(event) = stream.next().await {
        match event.msg {
            reflect::EventMsg::AgentMessageDelta(d) => print!("{}", d.delta),
            reflect::EventMsg::TurnComplete(t) => {
                println!(
                    "\n[turn complete: status={:?}, tokens in={}/out={}]",
                    t.status, t.usage.input_tokens, t.usage.output_tokens,
                );
                break;
            }
            reflect::EventMsg::Error(e) => {
                eprintln!("[error] {}: {}", e.code, e.message);
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
