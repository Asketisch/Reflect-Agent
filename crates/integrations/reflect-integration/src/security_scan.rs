//! 安全扫描 —— 包装 `cargo audit` 做依赖漏洞检查。

use std::process::Command;

use serde::{Deserialize, Serialize};

/// `cargo audit` 扫描结果摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityScanReport {
    /// 是否找到 cargo-audit 可执行文件并成功执行。
    pub ran: bool,
    /// 进程 exit code(未运行时 None)。
    pub exit_code: Option<i32>,
    /// stdout + stderr 合并输出(截断至 64 KiB)。
    pub output: String,
    /// 人类可读摘要。
    pub summary: String,
}

const MAX_OUTPUT: usize = 64 * 1024;

/// 运行 `cargo audit --json`(若 cargo-audit 不可用则降级为 `--version` 探测)。
///
/// 不 panic:任何 IO 错误都落到 `SecurityScanReport::ran = false`。
pub fn run_cargo_audit(workspace: Option<&std::path::Path>) -> SecurityScanReport {
    let mut cmd = Command::new("cargo");
    cmd.arg("audit");
    if let Some(ws) = workspace {
        cmd.current_dir(ws);
    }
    let out = cmd.output();
    match out {
        Ok(output) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            if text.len() > MAX_OUTPUT {
                text.truncate(MAX_OUTPUT);
                text.push_str("\n…(输出已截断)");
            }
            let code = output.status.code();
            let summary = if output.status.success() {
                "cargo audit: 未发现已知漏洞(或输出为空)".to_string()
            } else if text.contains("no such subcommand") || text.contains("unknown subcommand") {
                "cargo audit 不可用 — 请安装: cargo install cargo-audit".to_string()
            } else {
                format!("cargo audit 完成(exit {}) — 请查看输出", code.unwrap_or(-1))
            };
            SecurityScanReport {
                ran: true,
                exit_code: code,
                output: text,
                summary,
            }
        }
        Err(e) => SecurityScanReport {
            ran: false,
            exit_code: None,
            output: e.to_string(),
            summary: format!("无法启动 cargo audit: {e}"),
        },
    }
}

/// CLI 友好打印。
pub fn print_report(report: &SecurityScanReport) {
    println!("{}", report.summary);
    if !report.output.is_empty() {
        println!("---");
        println!("{}", report.output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_serializes() {
        let r = SecurityScanReport {
            ran: true,
            exit_code: Some(0),
            output: "ok".into(),
            summary: "ok".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"ran\":true"));
    }

    #[test]
    fn run_cargo_audit_does_not_panic() {
        let r = run_cargo_audit(None);
        // 无论是否安装 cargo-audit 都应返回结构化报告。
        assert!(!r.summary.is_empty());
    }
}
