//! Standalone `zhuye-exec` binary entry point.
//!
//! v0.4 起此文件**不再编译** —— 顶层 `zhuye` 二进制通过 `zhuye_exec::run`
//! 进程内转发。源文件保留以便 archaeology / IDE 跳转 / git blame。
//! 用户应改用 `zhuye exec "<prompt>"` / `zhuye exec -c` / `zhuye exec -r <N>`
//! 等顶层子命令。

use clap::Parser;
use zhuye_exec::{ExecArgs, run};

#[derive(Debug, Parser)]
#[command(name = "zhuye-exec", version, about = "Zhuye headless executable")]
struct Cli {
    #[command(flatten)]
    args: ExecArgs,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli.args)
}
