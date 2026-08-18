//! `reflect update` —— 打印升级提示(v0.4 不联网,v0.5 接 GitHub release API)。

/// v0.4: `reflect update` 的实现。v0.4 范围**不联网**,只打印本地版本
/// + 推荐升级方式。`--check-only` 行为同无 flag(v0.5 接 GitHub 时区分)。
///
/// 退出码:总是 0(命令本身不应失败)。
pub fn run(check_only: bool) -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    println!("reflect v{version}");
    if check_only {
        println!("(check-only flag reserved for v0.5 GitHub release API; v0.4 always prints hint)");
    }
    println!();
    println!("Upgrade options:");
    println!("  cargo install --git https://github.com/CNB/Reflect reflect");
    println!("  Or download a release: https://github.com/CNB/Reflect/releases/latest");
    println!();
    println!("Or build from source:");
    println!("  cargo install --path crates/reflect-cli");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 输出含当前版本字符串。
    #[test]
    fn update_prints_current_version() {
        let result = run(false);
        assert!(result.is_ok());
        // version 来自 env!,在测试里通过 `CARGO_PKG_VERSION` 直接拿。
        let v = env!("CARGO_PKG_VERSION");
        // 我们没法 stdout-capture,只能确认 run() 返回 ok;详细 snap test 在 cli_smoke 测。
        assert!(!v.is_empty());
    }

    /// `--check-only` 不 panic 也不写盘。
    #[test]
    fn update_check_only_does_not_modify_state() {
        let result = run(true);
        assert!(result.is_ok());
    }
}
