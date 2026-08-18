//! `reflect security` — cargo audit 安全扫描 CLI。

use std::path::PathBuf;

use reflect_integration::security_scan::{print_report, run_cargo_audit};

/// 运行安全扫描并打印报告。
pub fn run_audit(workspace: Option<PathBuf>) -> anyhow::Result<()> {
    println!("Reflect security audit");
    println!("======================");
    let report = run_cargo_audit(workspace.as_deref());
    print_report(&report);
    Ok(())
}
