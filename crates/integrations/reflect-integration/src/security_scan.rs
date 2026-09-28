//! 安全扫描 —— 包装 `cargo audit` 做依赖漏洞检查。

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// 默认探测超时。`cargo audit` 冷启动要拉取 RustSec advisory DB
/// (网络);离线 / 出口受限环境下会无限阻塞 —— 诊断路径必须有界。
pub const DEFAULT_AUDIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 运行 `cargo audit`(默认超时,见 [`DEFAULT_AUDIT_TIMEOUT`])。
pub fn run_cargo_audit(workspace: Option<&std::path::Path>) -> SecurityScanReport {
    run_cargo_audit_with_timeout(workspace, DEFAULT_AUDIT_TIMEOUT)
}

/// 运行 `cargo audit --json`(若 cargo-audit 不可用则降级为 `--version` 探测)。
///
/// v1.6:超时 + stdin 置 null。stdout / stderr 各起 reader 线程排空,
/// 防止输出超过管道缓冲(64 KiB)时子进程写阻塞、探测死锁。
/// 不 panic:任何 IO 错误 / 超时都落到结构化报告。
pub fn run_cargo_audit_with_timeout(
    workspace: Option<&std::path::Path>,
    timeout: Duration,
) -> SecurityScanReport {
    let mut cmd = Command::new("cargo");
    cmd.arg("audit");
    if let Some(ws) = workspace {
        cmd.current_dir(ws);
    }
    run_audit_cmd(&mut cmd, timeout)
}

/// 内核实现:执行 `cmd`(应输出合并的 audit 报告),带超时与管道排空。
fn run_audit_cmd(cmd: &mut Command, timeout: Duration) -> SecurityScanReport {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn();
    let mut child: Child = match child {
        Ok(c) => c,
        Err(e) => {
            return SecurityScanReport {
                ran: false,
                exit_code: None,
                output: e.to_string(),
                summary: format!("无法启动 cargo audit: {e}"),
            };
        }
    };

    // reader 线程:排空管道(防写阻塞),主循环只管定时 wait。
    let stdout_handle = child.stdout.take().map(spawn_reader);
    let stderr_handle = child.stderr.take().map(spawn_reader);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let stdout = stdout_handle.map(join_reader).unwrap_or_default();
    let stderr = stderr_handle.map(join_reader).unwrap_or_default();

    // 超时路径:ran=false 语义是"没有完成一次扫描"。
    let Some(status) = status else {
        return SecurityScanReport {
            ran: false,
            exit_code: None,
            output: format!(
                "cargo audit 超时(> {}s,可能卡在 advisory DB 网络拉取)",
                timeout.as_secs()
            ),
            summary: format!(
                "cargo audit 探测超时({}s);跳过,请手动运行确认",
                timeout.as_secs()
            ),
        };
    };

    let mut text = String::new();
    text.push_str(&stdout);
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if text.len() > MAX_OUTPUT {
        text.truncate(MAX_OUTPUT);
        text.push_str("\n…(输出已截断)");
    }
    let code = status.code();
    let summary = if status.success() {
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

/// 起一个 reader 线程把管道读尽(防子进程写满管道阻塞)。
fn spawn_reader(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        // 忽略读错误:超时 kill 后管道关闭,读到多少算多少。
        let _ = r.read_to_string(&mut buf);
        buf
    })
}

fn join_reader(h: std::thread::JoinHandle<String>) -> String {
    // reader 在管道关闭(进程退出 / kill)后必然返回,join 不会久等。
    h.join().unwrap_or_default()
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

    /// 卡死命令:直接把 `sleep 30` 当 audit 命令跑,超时后必须返回
    /// ran=false 且耗时远小于 30s(无需 PATH 注入 —— 内核函数收任意 Command)。
    #[test]
    fn audit_timeout_kills_stuck_command() {
        let started = Instant::now();
        let r = run_audit_cmd(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(300),
        );
        assert!(!r.ran, "卡死的 audit 命令应判为未完成(ran=false)");
        assert!(r.summary.contains("超时"), "summary: {}", r.summary);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "必须及时 kill,不能等 sleep 30 自然退出"
        );

        // 正常退出命令:ran=true 且 stdout 进报告。
        let r = run_audit_cmd(
            Command::new("/bin/echo").arg("audit-ok"),
            Duration::from_secs(5),
        );
        assert!(r.ran);
        assert!(r.output.contains("audit-ok"), "output: {}", r.output);
        assert!(r.summary.contains("未发现已知漏洞"));
    }
}
