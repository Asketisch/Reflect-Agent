//! `multi_turn` —— 三轮顺序对话,共用同一个 `Reflect` 实例。
//!
//! 演示历史在多轮之间延续,以及 agent 基于先前上下文
//! 逐步完善自己的回答。
//!
//! 运行:
//! ```bash
//! OPENAI_API_KEY=sk-... cargo run -p reflect --example multi_turn
//! ```

use reflect::{Reflect, Submission};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Reflect::builder("openai/gpt-4o").build()?;

    let turns = [
        "What is the capital of France?",
        "And what's the population of that city, roughly?",
        "Now summarize both answers in one sentence.",
    ];

    for (i, prompt) in turns.iter().enumerate() {
        let prompt: &str = prompt;
        println!("\n[USER {}] {}", i + 1, prompt);
        let mut stream = agent.submit(Submission::user_input(prompt)).await;
        print!("[AGENT] ");
        while let Some(event) = stream.next().await {
            match event.msg {
                reflect::EventMsg::AgentMessageDelta(d) => print!("{}", d.delta),
                reflect::EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
        println!();
    }
    Ok(())
}
