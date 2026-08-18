//! Standalone `reflect-exec` binary entry point.
//!
//! v0.4 起此文件**不再编译** —— 顶层 `reflect` 二进制通过 `reflect_exec::run`
//! 进程内转发。源文件保留以便 archaeology / IDE 跳转 / git blame。
//! 用户应改用 `reflect exec "<prompt>"` / `reflect exec -c` / `reflect exec -r <N>`
//! 等顶层子命令。

use clap::Parser;
use reflect_exec::{ExecArgs, run};

#[derive(Debug, Parser)]
#[command(name = "reflect-exec", version, about = "Reflect headless executable")]
struct Cli {
    #[command(flatten)]
    args: ExecArgs,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli.args)
}
