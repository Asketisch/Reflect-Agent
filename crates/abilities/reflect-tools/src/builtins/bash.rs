//! `bash` —— 在 workspace 中执行 shell 命令。
//!
//! 不并发安全(有副作用)。M3 将加入 `cwd` 沙箱、完整的环境变量白名单与审批集成;
//! M1 仅交付一个能用的最小版本。
//!
//! v1.1.0 P1 `bash-classifier`: [`classify_command`] 将命令分为
//! Safe / Risky / Dangerous,供 `ToolExecutionQueue` 动态设置审批风险等级。
//! 实现已下沉到 `reflect-hooks::bash_classify`(因 `PlanModeGate` 需复用,
//! 而依赖方向为 reflect-tools → reflect-hooks),此处 re-export 保持对外
//! API 不变。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::warn;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
pub use reflect_hooks::{BashCommandClass, classify_command};
use reflect_protocol::PermissionMode;

// `ToolError` 与 `ToolOutput` 通过 `crate::tool::*` 从 `reflect_protocol` 重新导出。

/// 每个流(stdout 或 stderr)在截断前最多捕获的字节数。
const OUTPUT_LIMIT: usize = 100 * 1024;

const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "USER", "SHELL", "TMPDIR"];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.bash")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "minimum": 0, "description": "Timeout in milliseconds (default 120000)"}
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // M6:bash 会改变外部状态;除非用户把它加入 session 白名单,否则
        // queue 会走 `ApprovalGate`。
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        // 弱模型(如 step-3.7-flash)常把命令放进 `command` / `cmdline` 而非
        // schema 声明的 `cmd`;流式 JSON 解析失败时 args 还会退化为 `Value::Null`
        // (见 `model_call/stream.rs` 的 `unwrap_or(Value::Null)`)。在此归一化:
        // 按 `cmd` → `command` → `cmdline` 顺序取首个非空字符串,schema 不变。
        // `.filter(!is_empty)` 一并挡掉 `{"cmd": ""}` / `{"cmd": "  "}` 与
        // `Value::Null` 退化两种静默失败,统一落到 `InvalidArgs`(沿用既有错误信息,
        // 不破坏 TUI 渲染 / 快照)。
        let cmd = ["cmd", "command", "cmdline"]
            .iter()
            .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'cmd'".into(),
            })?;

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);
        let timeout = Duration::from_millis(timeout_ms);

        // v1.3:OS 沙箱。`OsSandbox::default()` v1.3 起 enabled + strict,
        // `enforce_or_fail()` 集中决定 fail-closed 行为:
        // - 后端可用 → 走 Seatbelt 包裹 / Landlock pre_exec。
        // - strict + 后端不可用 / profile 写失败 → 返回
        //   `ToolError::SandboxUnavailable`,**绝不**回退到裸 `sh -c`。
        // - 非 strict + 后端不可用 → 保留 v1.2 降级透传语义(供
        //   REFLECT_SANDBOX_STRICT=0 显式开启)。
        let sandbox = reflect_sandbox::OsSandbox::from_env();
        let workspace = ctx.workspace_path();
        // 严格模式 fail-closed:后端 / 初始化失败 → 直接报错,不裸执行。
        if let Err(reason) = sandbox.enforce_or_fail() {
            warn!(error = %reason, "sandbox fail-closed; refusing bare exec");
            return Err(ToolError::SandboxUnavailable { reason });
        }
        // 决定 (program, argv):沙箱激活且为 Seatbelt → sandbox-exec 包裹;
        // 否则 → sh -c(Linux Landlock 在下方 pre_exec 应用)。
        // 注意:strict 模式下 `enforce_or_fail` 已确保 status 不会是
        // NoBackend / Disabled,所以这里只有"成功"和"非 strict 透传"两种分支。
        let (program, argv): (String, Vec<String>) = if matches!(
            sandbox.status(),
            reflect_sandbox::OsSandboxStatus::Seatbelt
        ) {
            match sandbox.seatbelt_argv(&workspace, &cmd) {
                Ok(pair) => pair,
                Err(e) => {
                    if sandbox.strict {
                        // strict + profile 写失败 → fail-closed,不裸执行。
                        let reason = format!("sandbox profile 生成失败: {e};严格模式下拒绝裸执行");
                        warn!(error = %e, "sandbox fail-closed on profile write");
                        return Err(ToolError::SandboxUnavailable { reason });
                    }
                    warn!(error = %e, "sandbox profile generation failed; falling back to unsandboxed exec");
                    ("sh".to_string(), vec!["-c".to_string(), cmd.clone()])
                }
            }
        } else {
            ("sh".to_string(), vec!["-c".to_string(), cmd.clone()])
        };
        // Seatbelt 模式下 argv[1] 是临时 .sb profile 路径;文件名按调用唯一
        // (见 seatbelt_argv),命令结束后由本工具负责删除,避免临时目录泄漏。
        let seatbelt_profile_path: Option<std::path::PathBuf> = if program == "/usr/bin/sandbox-exec"
        {
            argv.get(1).map(std::path::PathBuf::from)
        } else {
            None
        };
        let mut command = Command::new(&program);
        command.args(&argv);
        // Linux Landlock:在 fork 后、exec 前应用规则(workspace 内放行,
        // 其余写拒)。内核不支持时降级(闭包内 Ok,不阻塞)。
        #[cfg(target_os = "linux")]
        if matches!(sandbox.status(), reflect_sandbox::OsSandboxStatus::Landlock) {
            use std::os::unix::process::CommandExt;
            let ws_clone = workspace.clone();
            let mut pre_exec = sandbox.landlock_pre_exec(ws_clone);
            unsafe {
                command.pre_exec(move || pre_exec());
            }
        }
        command
            .current_dir(&workspace)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);

        for key in ENV_WHITELIST {
            if let Ok(v) = std::env::var(key) {
                command.env(key, v);
            }
        }

        let mut child = command.spawn().map_err(ToolError::from)?;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let kill_on_cancel = ctx.cancel.clone();
        // M3:持有可变 child 句柄,drain 完 stdout/stderr 后 `wait()` 取 exit code。
        // 此前 M1 实现靠 `kill_on_drop` 让子进程自生自灭,从不 wait → 拿不到
        // exit code,`is_error` 恒为 false。现在用 `Option<Child>` 包住,在
        // drain 后(仍在 timeout 内)调 `wait().await` 真正 reap 并读 status。
        let mut child_handle = Some(child);

        let result = tokio::time::timeout(timeout, async {
            // Concurrently drain both pipes while waiting.
            let stdout_fut = async {
                let mut buf = Vec::with_capacity(8192);
                if let Some(s) = stdout.as_mut() {
                    let _: tokio::io::Take<&mut tokio::process::ChildStdout> =
                        s.take(OUTPUT_LIMIT as u64);
                    let _ = s.read_to_end(&mut buf).await;
                }
                buf
            };
            let stderr_fut = async {
                let mut buf = Vec::with_capacity(8192);
                if let Some(s) = stderr.as_mut() {
                    let _: tokio::io::Take<&mut tokio::process::ChildStderr> =
                        s.take(OUTPUT_LIMIT as u64);
                    let _ = s.read_to_end(&mut buf).await;
                }
                buf
            };
            let (out_bytes, err_bytes) = tokio::join!(stdout_fut, stderr_fut);
            // Reap the child & capture exit code(M3:此前 M1 从不 wait)。
            // drain 已把 stdout/stderr 读空,wait 通常立即返回。
            let exit_code = if let Some(mut ch) = child_handle.take() {
                match ch.wait().await {
                    Ok(status) => status.code().unwrap_or(-1),
                    Err(_) => -1,
                }
            } else {
                -1
            };
            (out_bytes, err_bytes, exit_code)
        });

        let (out_bytes, err_bytes, exit_code) = match result.await {
            Ok(triple) => triple,
            Err(_) => {
                warn!(cmd, "bash tool timed out");
                if let Some(p) = &seatbelt_profile_path {
                    let _ = std::fs::remove_file(p);
                }
                return Err(ToolError::Timeout {
                    elapsed_ms: timeout_ms,
                });
            }
        };
        // 命令已结束(正常或被 reap),临时 .sb profile 不再需要,删除。
        if let Some(p) = &seatbelt_profile_path {
            let _ = std::fs::remove_file(p);
        }

        // M3:child 已在 timeout 块内 `wait()` reap,exit_code 已捕获。
        // 若 timeout 触发,上面 `Err(_)` 分支已 return;若 child_handle 仍在
        // (理论上不会,drain+wait 在块内完成),靠 `kill_on_drop` 兜底。
        let _ = kill_on_cancel;

        let stdout_str = String::from_utf8_lossy(&out_bytes).to_string();
        let stderr_str = String::from_utf8_lossy(&err_bytes).to_string();

        let mut text = stdout_str;
        if !stderr_str.is_empty() {
            if !text.is_empty() {
                text.push_str("\nstderr:\n");
            }
            text.push_str(&stderr_str);
        }
        if text.len() > OUTPUT_LIMIT {
            text.truncate(OUTPUT_LIMIT);
            text.push_str("\n... [truncated]");
        }

        // M3:exit_code 进 metadata,TUI reducer 据此渲染 `ExecResult` 单元格;
        // 非零 exit code 标记 `is_error = true`(此前恒 false,无法区分失败命令)。
        // exit_code == -1 表示被信号杀死或 wait 失败(无法取 status),按错误处理。
        let is_error = exit_code != 0;
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::text(text)],
            is_error,
            metadata: serde_json::json!({
                "stdout_bytes": out_bytes.len(),
                "stderr_bytes": err_bytes.len(),
                "exit_code": exit_code,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注:`classify_command` / `BashCommandClass` 的分类单测已随实现一起
    // 搬到 `reflect-hooks::bash_classify`。本模块只保留 BashTool 执行行为测试。

    #[tokio::test]
    async fn echo_runs_and_captures_stdout() {
        let t = BashTool;
        let ctx = ToolContext::for_workspace(".");
        let out = t
            .execute(ctx, serde_json::json!({"cmd": "echo hello"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("hello"), "got: {text}");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn bash_captures_exit_code_and_marks_failure() {
        // M3:成功命令 exit_code=0 + is_error=false;失败命令 exit_code=非0 + is_error=true。
        // metadata.exit_code 应被填充,供 TUI reducer 渲染 ExecResult 单元格。
        let t = BashTool;
        // 成功命令。
        let ok = t
            .execute(
                ToolContext::for_workspace("."),
                serde_json::json!({"cmd": "true"}),
            )
            .await
            .unwrap();
        assert_eq!(
            ok.metadata.get("exit_code"),
            Some(&serde_json::json!(0)),
            "exit 0 命令应记录 exit_code=0"
        );
        assert!(!ok.is_error, "exit 0 不应标记为 error");
        // 失败命令。
        let fail = t
            .execute(
                ToolContext::for_workspace("."),
                serde_json::json!({"cmd": "false"}),
            )
            .await
            .unwrap();
        assert_eq!(
            fail.metadata.get("exit_code"),
            Some(&serde_json::json!(1)),
            "exit 1 命令应记录 exit_code=1"
        );
        assert!(fail.is_error, "exit 1 应标记为 error");
    }

    #[tokio::test]
    async fn rejects_missing_cmd() {
        let t = BashTool;
        let err = t
            .execute(ToolContext::default(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    // ── 参数别名归一化(弱模型常把命令放进 command 而非 cmd) ──

    #[tokio::test]
    async fn bash_accepts_command_alias() {
        // 模型误用 `command` 而非 schema 声明的 `cmd`;归一化后应能执行。
        let t = BashTool;
        let out = t
            .execute(
                ToolContext::for_workspace("."),
                serde_json::json!({"command": "echo via-command-alias"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "alias should execute");
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("via-command-alias"), "got: {text}");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_cmd() {
        // `{"cmd": ""}` / `{"cmd": "   "}` 视同缺失 —— trim 后为空。
        let t = BashTool;
        let err = t
            .execute(ToolContext::default(), serde_json::json!({"cmd": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_null_args() {
        // 模拟流式 JSON 解析失败退化为 Value::Null 的场景。
        let t = BashTool;
        let err = t
            .execute(ToolContext::default(), serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn bash_cmd_still_primary_over_command() {
        // 当 cmd 与 command 同时存在,必须用 schema 的 cmd,绝不被 command 污染
        // (防止别名归一化引入「命令注入/覆盖」回归)。
        let t = BashTool;
        let out = t
            .execute(
                ToolContext::for_workspace("."),
                serde_json::json!({
                    "cmd": "echo from-cmd",
                    "command": "echo from-command-should-not-run"
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => {
                assert!(text.contains("from-cmd"), "got: {text}");
                assert!(
                    !text.contains("should-not-run"),
                    "command alias must not override cmd: {text}"
                );
            }
            _ => panic!("expected text"),
        }
    }

    // v1.2 P0-1:沙箱集成测试。`std::env::set_var` 在 Rust 2024 是 unsafe,
    // 用模块级锁串行化避免并行测试竞态(镜像 reflect-core::config 的 pattern)。
    //
    // 持锁设计:下方三个 sandbox 测试在 `execute().await` 全程持有
    // `SANDBOX_ENV_LOCK` —— 这是**故意**的,因为 execute 内部会读
    // `REFLECT_SANDBOX_OS_LEVEL` / `REFLECT_SANDBOX_STRICT`,若并发测试在
    // execute 期间改写 env 会读到错误值。`std::sync::MutexGuard` 非 Send,
    // clippy `await_holding_lock` 会警告;但 `#[tokio::test]` 默认
    // current_thread runtime,guard 不跨线程,无死锁风险。故各测试加
    // `#[allow(clippy::await_holding_lock)]` 显式表达「故意全程持锁」。
    static SANDBOX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// gap doc 核心验证用例:启用沙箱后,写系统目录(`/usr/local/...`)必须
    /// 被内核拒绝。BashTool 读 `REFLECT_SANDBOX_OS_LEVEL=1` 激活 Seatbelt
    /// (macOS),`sandbox-exec` 限制 fs 写。命令尝试 `touch` 一个系统路径,
    /// 沙箱应让该写失败 → 文件不存在 → 输出 BLOCKED。
    ///
    /// 仅 macOS:Linux Landlock 需真实内核支持 + 测试进程不能已 restrict,
    /// 留集成测试覆盖。
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 见 SANDBOX_ENV_LOCK 注释:故意全程持锁串行化 env
    async fn bash_sandbox_blocks_write_to_system_dir() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "1");
        }
        // 用唯一标记文件,测试后清理(沙箱下创建失败也无妨)。
        let marker = "/usr/local/.reflect-bash-sandbox-test";
        let cmd =
            format!("touch {marker} 2>/dev/null; test -f {marker} && echo WROTE || echo BLOCKED");
        let t = BashTool;
        let ctx = ToolContext::for_workspace("/tmp");
        let out = t
            .execute(ctx, serde_json::json!({"cmd": cmd, "timeout_ms": 15000}))
            .await
            .unwrap();
        // 还原 env(无论断言是否通过)。
        match prior {
            Some(p) => unsafe {
                std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p);
            },
            None => unsafe {
                std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL");
            },
        }
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("BLOCKED"),
            "sandbox must block write to /usr/local; got: {text}"
        );
        // 清理(若沙箱失效误创建了文件)。
        let _ = std::fs::remove_file(marker);
    }

    /// v1.3:sandbox 关闭(显式 env)时,普通命令正常透传(向后兼容)。
    /// v1.3 起默认要求 sandbox,显式 `REFLECT_SANDBOX_OS_LEVEL=0` 才
    /// 关闭;同时 `REFLECT_SANDBOX_STRICT=0` 关闭严格模式可让 fail-closed
    /// 路径降级透传(本测试两条都设)。
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 见 SANDBOX_ENV_LOCK 注释:故意全程持锁串行化 env
    async fn bash_sandbox_off_runs_normally() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
        }
        let t = BashTool;
        let ctx = ToolContext::for_workspace("/tmp");
        let out = t
            .execute(ctx, serde_json::json!({"cmd": "echo unsandboxed-ok"}))
            .await
            .unwrap();
        match prior_on {
            Some(p) => unsafe {
                std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p);
            },
            None => unsafe {
                std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL");
            },
        }
        match prior_strict {
            Some(p) => unsafe {
                std::env::set_var("REFLECT_SANDBOX_STRICT", p);
            },
            None => unsafe {
                std::env::remove_var("REFLECT_SANDBOX_STRICT");
            },
        }
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("unsandboxed-ok"), "got: {text}");
    }

    /// v1.3 fail-closed 不变式:strict + 后端不可用时 BashTool 必须
    /// 返回 `ToolError::SandboxUnavailable`,**绝不**裸执行。
    /// 在没有真实后端的环境里(如 Linux 旧内核 / 容器里
    /// `sandbox-exec` 缺失),`from_env` 返回 `NoBackend`,fail-closed
    /// 路径触发。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 见 SANDBOX_ENV_LOCK 注释:故意全程持锁串行化 env
    async fn bash_fail_closed_when_strict_and_no_backend() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            // 强制启用 sandbox + strict,模拟「默认基线」。
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "1");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "1");
        }
        let t = BashTool;
        let ctx = ToolContext::for_workspace("/tmp");
        let out = t
            .execute(ctx, serde_json::json!({"cmd": "echo should-not-run"}))
            .await;
        match prior_on {
            Some(p) => unsafe {
                std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p);
            },
            None => unsafe {
                std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL");
            },
        }
        match prior_strict {
            Some(p) => unsafe {
                std::env::set_var("REFLECT_SANDBOX_STRICT", p);
            },
            None => unsafe {
                std::env::remove_var("REFLECT_SANDBOX_STRICT");
            },
        }
        match out {
            // 后端可用(macOS 有 sandbox-exec / Linux 有 Landlock)
            // → 命令在沙箱里跑成功。
            Ok(_) => {}
            // 后端不可用 → fail-closed 返回 SandboxUnavailable。
            Err(ToolError::SandboxUnavailable { reason }) => {
                assert!(!reason.is_empty(), "fail-closed reason must be non-empty");
            }
            // 其他错误 → 不应出现。
            Err(other) => {
                panic!("strict 模式下后端不可用应返回 SandboxUnavailable,得到: {other:?}")
            }
        }
    }
}
