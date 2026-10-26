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
//!
//! v1.7 持久 shell 状态:同一 [`BashTool`] 实例(= 同一 session 的
//! registry)跨调用保留工作目录 —— 用户命令被包进 `{ ...; }` 组,组后用
//! sentinel 行回报 `$?` 与 `$PWD`(同时打到 stdout 与 stderr:单一流被
//! 输出上限截断时另一条仍存活),工具解析后存入 `workspace → cwd` 映射;
//! 下次调用前置 `cd` 恢复。目录丢失时回落 workspace 根并提示。
//! `run_in_background` 的命令同样前置 `cd`,但不更新持久状态(后台进程
//! 不代表前台 shell 的去向)。命令被超时/取消杀死时拿不到 sentinel,
//! 保持旧状态。sentinel 行对模型不可见(解析后从两条流中剥除;剥除后
//! 恢复一个尾部换行,保持与未包裹时字节语义一致)。输出超过
//! [`OUTPUT_LIMIT`] 截断时,把已捕获的完整输出落盘到临时文件并在输出
//! 末尾给出路径,供模型用 read 工具取回。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::process::Command;
use tracing::{error, warn};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
pub use reflect_hooks::{BashCommandClass, classify_command};
use reflect_protocol::PermissionMode;

// `ToolError` 与 `ToolOutput` 通过 `crate::tool::*` 从 `reflect_protocol` 重新导出。

/// 每个流(stdout 或 stderr)在截断前最多捕获的字节数。
const OUTPUT_LIMIT: usize = 100 * 1024;

const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "USER", "SHELL", "TMPDIR"];

/// v1.7 持久 cwd:sentinel 行前缀。命令组结束后由 `printf` 打到 stdout
/// 的一行,格式 `__REFLECT_STATE__ rc=<n> cwd=<path>`(cwd 可含空格,
/// 取行内 `cwd=` 之后的全部内容)。刻意冷门化,避免与真实输出撞行。
const SENTINEL_PREFIX: &str = "__REFLECT_STATE__ ";

/// 持久 shell 状态的工作目录。`BashTool` 实例按注册周期存活
/// (builder / headless 每个 session 构造一个 registry),实例字段即
/// session 级状态:workspace 路径 → 该 workspace 下最近一次命令结束时的
/// `$PWD`。`enter_worktree` 热切换 workspace 后自然分键,互不串扰。
#[derive(Default)]
pub struct BashTool {
    shell_cwd: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
}

impl BashTool {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 单引号 shell 转义(`'` → `'\''`),用于 `cd -- '<path>'` 前缀。
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// v1.7:从 stdout 字节中定位并解析 sentinel 行。
///
/// 返回 `(解析结果, 剥除 sentinel 后的输出)`。解析结果:
/// `Some((rc, cwd))` 当最后一行形如 `__REFLECT_STATE__ rc=<n> cwd=<path>`;
/// 找不到 / 格式不符时 `None`(输出原样返回)。
///
/// 剥除规则:sentinel 由 `printf '\n__REFLECT_STATE__ ...'` 追加,永远
/// 独占一行且前面多一个 `\n`;剥除该行后把输出尾部多出的一个 `\n`
/// 去掉,字节语义与未包裹版本对齐。
fn extract_shell_state(out: &[u8]) -> (Option<(i32, String)>, Vec<u8>) {
    let prefix = SENTINEL_PREFIX.as_bytes();
    // 从后往前找「行首 + 前缀」的最后一次出现(用户输出也可能包含该
    // 字面串,取最后一行保证命中的是我们追加的那个)。
    let mut start = None;
    for i in (0..out.len()).rev() {
        if (i == 0 || out[i - 1] == b'\n') && out[i..].starts_with(prefix) {
            start = Some(i);
            break;
        }
    }
    let Some(start) = start else {
        return (None, out.to_vec());
    };
    let line_end = out[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| start + p)
        .unwrap_or(out.len());
    let line = String::from_utf8_lossy(&out[start..line_end]);
    let rest = &line[SENTINEL_PREFIX.len()..];
    let parsed = parse_state_fields(rest);
    // 剥除 sentinel 行(含行尾换行),再补回被 printf 前置 `\n` 多出的
    // 一个换行,恢复原始输出形态。
    let mut cleaned: Vec<u8> = Vec::with_capacity(out.len());
    cleaned.extend_from_slice(&out[..start]);
    if line_end < out.len() {
        cleaned.extend_from_slice(&out[line_end + 1..]);
    }
    if parsed.is_some() && cleaned.last() == Some(&b'\n') {
        cleaned.pop();
    }
    (parsed, cleaned)
}

/// 解析 sentinel 行的 `rc=<n> cwd=<path>` 字段。cwd 取 `cwd=` 之后的
/// 整段(允许含空格);rc 或 cwd 缺失 / rc 非整数 → `None`。
fn parse_state_fields(rest: &str) -> Option<(i32, String)> {
    let rc = rest
        .split_whitespace()
        .find_map(|t| t.strip_prefix("rc="))
        .and_then(|v| v.parse::<i32>().ok())?;
    let cwd = rest.split_once("cwd=")?.1.trim_end().to_string();
    Some((rc, cwd))
}

/// v1.4 A3:收割单个输出流(stdout / stderr)。
///
/// - `progress = None`:一次性 `read_to_end`(历史路径,零额外开销);
/// - `progress = Some`:按行 `read_until(b'\n')` 逐行回调(字节级,容忍
///   非 UTF-8 输出——`read_line` 遇无效 UTF-8 会报错中断,这里用字节
///   缓冲 + 送达回调时做 lossy 转换)。两条路径都受 `OUTPUT_LIMIT`
///   总量限制,最终 `ToolOutput` 的完整文本语义一致(增量只是预览)。
///
/// review 修复:捕获到 `OUTPUT_LIMIT` 上限后必须**继续排空管道(丢弃
/// 剩余输出)直至 EOF** —— `Take` 到达限额后返回 EOF 语义,若就此停读,
/// 子进程把 pipe 缓冲写满后会永久阻塞在 write 上,`wait()` 挂死直到外层
/// timeout 把命令误杀(表现为「输出超限的命令必然超时」)。
async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    progress: Option<&crate::tool::ProgressSink>,
    is_stderr: bool,
) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(8192);
    let Some(pipe) = pipe else {
        return buf;
    };
    let mut limited = pipe.take(OUTPUT_LIMIT as u64);
    match progress {
        None => {
            let _ = limited.read_to_end(&mut buf).await;
            drain_after_cap(&mut limited).await;
        }
        Some(sink) => {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(limited);
            let mut line: Vec<u8> = Vec::new();
            // v1.7:sentinel 行不进增量流。sentinel 由 `printf '\n__REFLECT…'`
            // 追加,前面恒有一个空 "\n" 行 —— 用「延迟一行」缓冲:当前行
            // 先扣住,看到下一行再发;若下一行是 sentinel,则把扣住的空行
            // 一并吞掉,保证增量视图与最终剥除后的输出完全一致(否则 TUI
            // 会闪现协议行 / 多余空行)。
            let mut pending: Option<Vec<u8>> = None;
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // sentinel 只从增量事件过滤;完整捕获必须保留,供解析退出码。
                        buf.extend_from_slice(&line);
                        if line.starts_with(SENTINEL_PREFIX.as_bytes()) {
                            if pending.as_deref() == Some(b"\n".as_slice()) {
                                pending = None;
                            }
                            continue;
                        }
                        if let Some(p) = pending.take() {
                            sink.emit(is_stderr, &String::from_utf8_lossy(&p));
                        }
                        pending = Some(line.clone());
                    }
                }
            }
            if let Some(p) = pending.take() {
                sink.emit(is_stderr, &String::from_utf8_lossy(&p));
            }
            drain_after_cap(reader.get_mut()).await;
        }
    }
    buf
}

/// 捕获满后继续排空管道(丢弃剩余输出)直至 EOF,防子进程写满 pipe
/// 缓冲后阻塞在 write 上(见 [`drain_pipe`] 的 review 修复说明)。
async fn drain_after_cap<T: tokio::io::AsyncRead + Unpin>(limited: &mut tokio::io::Take<T>) {
    use tokio::io::AsyncReadExt;
    let mut discard = [0u8; 8192];
    loop {
        match limited.get_mut().read(&mut discard).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

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
                "timeout_ms": {"type": "integer", "minimum": 0, "description": "Timeout in milliseconds (default 120000)"},
                "run_in_background": {"type": "boolean", "description": "Run in background: return a task id immediately; output is injected at the next turn boundary (query via background_status)"}
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
        //
        // 0.3:该规范化收敛到 `crate::bash_command::resolve_bash_command`,
        // 与 queue / approval gate / PlanModeGate 等**所有安全判定点**共用
        // 同一实现 —— 此前这些点各自 `args.get("cmd")`,模型用 `command`
        // 别名就能让整条风险管线静默降级(详见 bash_command.rs 模块文档)。
        let cmd = crate::bash_command::resolve_bash_command(&args)
            .map(|s| s.to_string())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'cmd'".into(),
            })?;

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);
        let timeout = Duration::from_millis(timeout_ms);

        let workspace = ctx.workspace_path();

        // v1.7 持久 shell 状态:取本 workspace 记录的上次 `$PWD`。目录已
        // 不存在(被删 / worktree 已清理)→ 清除记录,回落 workspace 根。
        let saved_cwd = {
            let mut map = self.shell_cwd.lock();
            match map.get(&workspace).cloned() {
                Some(c) if c.as_path().exists() => Some(c),
                Some(_) => {
                    map.remove(&workspace);
                    None
                }
                None => None,
            }
        };
        // 命令执行前的有效 cwd(用于判断本次命令是否移动了目录)。
        let effective_prev = saved_cwd
            .clone()
            .or_else(|| std::fs::canonicalize(&workspace).ok());

        // v1.5 R2:后台执行 —— 立即返回任务 id,输出经队列在下一回合
        // 边界注入(或 background_status 查询)。宿主未接后台生成器时
        // 明确报错,不静默降级为前台执行。OS 沙箱覆盖随调用透传,由
        // 宿主侧与前台同等执行(严格模式 fail-closed)。
        // 后台命令同样从持久 cwd 起跑(前置 `cd`),但**不**写回状态:
        // 后台进程的去向不代表前台 shell。
        if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true) {
            let spawner = ctx
                .background
                .as_ref()
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "run_in_background: 本线程未接入后台任务生成器".into(),
                })?;
            let bg_cmd = match &saved_cwd {
                // 同样不依赖 `2>/dev/null`(见下方 script 处的注释):
                // Landlock 下写 /dev/null 被拒会让重定向失败并中止命令。
                Some(c) => format!(
                    "if [ -d {q} ]; then cd -- {q}; fi; {cmd}",
                    q = shell_quote(c)
                ),
                None => cmd.clone(),
            };
            let task_id = spawner.spawn_bash(&bg_cmd, &ctx.workspace_path(), ctx.os_sandbox)?;
            return Ok(ToolOutput {
                content: vec![reflect_protocol::ContentBlock::text(format!(
                    "[background task {task_id} started] 命令已在后台执行;完成结果将在下一个回合边界自动注入,也可用 background_status 查询。"
                ))],
                is_error: false,
                metadata: serde_json::json!({
                    "background": true,
                    "task_id": task_id,
                }),
                elapsed_ms: 0,
            });
        }

        // v1.7:把用户命令包进 `{ ...; }` 组,组后由 sentinel 行回报退出码
        // 与 `$PWD`(parse 失败 / shell 提前退出时无 sentinel,退出码与错误
        // 信息语义与未包裹版本一致)。前置 `cd` 恢复持久 cwd,恢复失败仅
        // 提示 stderr、不阻断执行。
        let mut script = String::new();
        if let Some(c) = &saved_cwd {
            // 恢复持久 cwd。**刻意不依赖 `>/dev/null`**:Linux Landlock
            // 沙箱此前未放行 /dev/null 写(只有 macOS Seatbelt 放行),
            // 重定向失败时 sh 报 "cannot create /dev/null: Permission
            // denied" 并**中止整条命令** —— cd 根本没执行,目录明明还在
            // 却被误判成"已不存在"(CI 实测:持久 cwd 跨调用失效)。
            // 改用 `test -d` 判定:目录没了才提示回落,目录在就直接 cd。
            script.push_str(&format!(
                "if [ -d {q} ]; then cd -- {q}; else printf '%s\\n' \"reflect: 持久工作目录 {d} 已不存在,回落到 workspace 根\" >&2; fi\n",
                q = shell_quote(c),
                d = c.display()
            ));
        }
        script.push_str(&format!(
            "{{ {cmd}\n}} ; __reflect_rc=$? ; printf '\\n{SENTINEL_PREFIX}rc=%s cwd=%s' \"$__reflect_rc\" \"$PWD\" ; printf '\\n{SENTINEL_PREFIX}rc=%s cwd=%s' \"$__reflect_rc\" \"$PWD\" >&2 ; exit \"$__reflect_rc\"\n"
        ));

        // v1.3:OS 沙箱。`OsSandbox::default()` v1.3 起 enabled + strict,
        // `enforce_or_fail()` 集中决定 fail-closed 行为:
        // - 后端可用 → 走 Seatbelt 包裹 / Landlock pre_exec。
        // - strict + 后端不可用 / profile 写失败 → 返回
        //   `ToolError::SandboxUnavailable`,**绝不**回退到裸 `sh -c`。
        // - 非 strict + 后端不可用 → 保留 v1.2 降级透传语义(供
        //   REFLECT_SANDBOX_STRICT=0 显式开启)。
        // v1.5 R1:OS 沙箱来源优先级 —— ToolContext 覆盖(`sandbox_policy`
        // 每 turn 下发)> env(`OsSandbox::from_env`)。
        let sandbox = match ctx.os_sandbox {
            Some(on) => reflect_sandbox::OsSandbox::with_enabled(on),
            None => reflect_sandbox::OsSandbox::from_env(),
        };
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
            match sandbox.seatbelt_argv(&workspace, &script) {
                Ok(pair) => pair,
                Err(e) => {
                    if sandbox.strict {
                        // strict + profile 写失败 → fail-closed,不裸执行。
                        let reason = format!("sandbox profile 生成失败: {e};严格模式下拒绝裸执行");
                        warn!(error = %e, "sandbox fail-closed on profile write");
                        return Err(ToolError::SandboxUnavailable { reason });
                    }
                    warn!(error = %e, "sandbox profile generation failed; falling back to unsandboxed exec");
                    ("sh".to_string(), vec!["-c".to_string(), script.clone()])
                }
            }
        } else {
            ("sh".to_string(), vec!["-c".to_string(), script.clone()])
        };
        // Seatbelt 模式下 argv[1] 是临时 .sb profile 路径;文件名按调用唯一
        // (见 seatbelt_argv),命令结束后由本工具负责删除,避免临时目录泄漏。
        let seatbelt_profile_path: Option<std::path::PathBuf> =
            if program == "/usr/bin/sandbox-exec" {
                argv.get(1).map(std::path::PathBuf::from)
            } else {
                None
            };
        let mut command = Command::new(&program);
        command.args(&argv);
        // Linux Landlock:在 fork 后、exec 前应用规则(workspace 内放行,
        // 其余写拒)。内核不支持时降级(闭包内 Ok,不阻塞)。
        // 注意:`pre_exec` 来自 tokio::process::Command 的固有方法,
        // 无需(也不能)导入 std 的 CommandExt —— 否则 Linux 上
        // `-D warnings` 会报 unused import(macOS 下该 cfg 块不编译,
        // 本地发现不了;CI ubuntu 实测踩坑)。
        //
        // v1.6 降级重试:受限环境(GHA runner 等)的 landlock 实现存在
        // 异常 —— restrict 成功后 execve 仍可能 EACCES,且**间歇出现**。
        // spawn 报权限类错误时:重建 command(不注入 pre_exec)降级重试
        // 一次,对齐「沙箱尽力而为,不阻塞工作流」的既有哲学。
        //
        // **0.1 收口(安全)**:该重试此前无条件生效,等于给了一个稳定的
        // 沙箱逃逸口子 —— 只要能让 sandboxed spawn 失败一次,后续命令就
        // 全部裸执行,且只有 stderr 一行 warn,headless JSONL 客户端完全
        // 看不到。现在改为**仅非 strict 模式允许**;strict 模式下这条
        // 路径直接 fail-closed 抛 `SandboxUnavailable`,与模块不变式
        // (os_sandbox.rs:22-27「strict 绝不回退裸 sh -c」)一致。
        // 另外 0.1 已修正 handled 位掩码(0xffe → 0x1ff2):EACCES 的
        // 根因是 READ_FILE/READ_DIR 被误纳入 handled,导致 execve 时
        // 动态链接器读不到 workspace 外的 .so,修好后这类失败本就不该
        // 再出现,重试也就从"必需品"退化为"纯逃逸面"。
        #[cfg(target_os = "linux")]
        let landlock_applied =
            matches!(sandbox.status(), reflect_sandbox::OsSandboxStatus::Landlock);
        #[cfg(not(target_os = "linux"))]
        let landlock_applied = false;
        // 非 strict 才允许降级重试(strict → fail-closed)。
        #[cfg(target_os = "linux")]
        let allow_degrade_retry = landlock_applied && !sandbox.strict;
        #[cfg(not(target_os = "linux"))]
        let allow_degrade_retry = false;
        #[cfg(target_os = "linux")]
        if landlock_applied {
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

        let mut child = match command.spawn() {
            Ok(c) => c,
            // 权限类失败 + landlock 已注入 + **非 strict** → 降级重试一次
            // (不带沙箱)。strict 模式走下面的 fail-closed 分支。
            Err(e)
                if allow_degrade_retry
                    && matches!(
                        e.raw_os_error(),
                        Some(13) | Some(1) // EACCES / EPERM(避免为此引 libc 依赖)
                    ) =>
            {
                warn!(
                    cmd,
                    error = %e,
                    "landlock sandboxed spawn failed (EACCES/EPERM); \
                     retrying without sandbox (non-strict mode)"
                );
                let mut command = Command::new(&program);
                command.args(&argv);
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
                command.spawn().map_err(ToolError::from)?
            }
            // strict 模式下的权限类失败:fail-closed,绝不裸执行。
            Err(e)
                if landlock_applied
                    && sandbox.strict
                    && matches!(e.raw_os_error(), Some(13) | Some(1)) =>
            {
                let reason = format!(
                    "landlock sandboxed spawn 失败({e});严格模式下拒绝降级为无沙箱执行。\
                     若运行环境确实无法在 landlock 下 exec,请显式设置 \
                     REFLECT_SANDBOX_STRICT=0 或 REFLECT_SANDBOX_OS_LEVEL=0。"
                );
                error!(
                    cmd,
                    error = %e,
                    "sandbox fail-closed; refusing to retry without sandbox"
                );
                if let Some(p) = &seatbelt_profile_path {
                    let _ = std::fs::remove_file(p);
                }
                return Err(ToolError::SandboxUnavailable { reason });
            }
            Err(e) => return Err(ToolError::from(e)),
        };
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let kill_on_cancel = ctx.cancel.clone();
        // v1.4 A3:有进度回调时逐行流式上报(经 queue 转发为
        // ToolCallOutputDelta),无回调时保持原 read_to_end 路径不变。
        let progress = ctx.progress.clone();
        // M3:持有可变 child 句柄,drain 完 stdout/stderr 后 `wait()` 取 exit code。
        // 此前 M1 实现靠 `kill_on_drop` 让子进程自生自灭,从不 wait → 拿不到
        // exit code,`is_error` 恒为 false。现在用 `Option<Child>` 包住,在
        // drain 后(仍在 timeout 内)调 `wait().await` 真正 reap 并读 status。
        let mut child_handle = Some(child);

        let result = tokio::time::timeout(timeout, async {
            // v1.4 C2:取消令牌贯通 —— select! biased 监听 ctx.cancel,
            // 触发时 start_kill 子进程并 reap(此前 kill_on_cancel 是死变量,
            // 中断 / 关闭只取消 LLM 流,正在跑的 bash 进程会跑满超时)。
            // 被 kill 的命令 exit_code 落到信号杀死路径(-1 / None)。
            tokio::select! {
                biased;
                _ = kill_on_cancel.cancelled() => {
                    if let Some(mut ch) = child_handle.take() {
                        let _ = ch.start_kill();
                        let _ = ch.wait().await;
                    }
                    (Vec::new(), Vec::new(), -1)
                }
                triple = async {
                    // Concurrently drain both pipes while waiting.
                    let stdout_fut = drain_pipe(stdout.as_mut(), progress.as_ref(), false);
                    let stderr_fut = drain_pipe(stderr.as_mut(), progress.as_ref(), true);
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
                } => triple,
            }
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

        // v1.7:解析 sentinel(退出码 + 最终 `$PWD`),并把该行从两条流的
        // 输出中剥除(sentinel 同时打到 stdout 与 stderr —— 单一流被
        // OUTPUT_LIMIT 截断时另一条仍存活)。两处都缺失(shell 被 exit/
        // 杀死 / 解析错误提前终止)→ 沿用进程退出码,持久状态不动。
        // 包装脚本也以原命令退出码结束,两条流都截断而丢失状态行时仍能正确回退。
        let (state_out, cleaned_out) = extract_shell_state(&out_bytes);
        let (state_err, cleaned_err) = extract_shell_state(&err_bytes);
        let shell_state = state_out.or(state_err);
        let exit_code = shell_state.as_ref().map_or(exit_code, |(rc, _)| *rc);

        // v1.7:最终 cwd 写回持久状态(仅接受绝对路径;`$PWD` 为空或相对
        // 视为异常,保持旧值)。
        let final_cwd: Option<String> = shell_state
            .as_ref()
            .map(|(_, c)| c.clone())
            .filter(|c| c.starts_with('/'));
        if let Some(cwd) = &final_cwd {
            self.shell_cwd
                .lock()
                .insert(workspace.clone(), PathBuf::from(cwd));
        }

        let stdout_str = String::from_utf8_lossy(&cleaned_out).to_string();
        let stderr_str = String::from_utf8_lossy(&cleaned_err).to_string();

        let mut text = stdout_str;
        if !stderr_str.is_empty() {
            if !text.is_empty() {
                text.push_str("\nstderr:\n");
            }
            text.push_str(&stderr_str);
        }
        // v1.7:命令移动了工作目录 → 在输出末尾追加一行提示,让模型在
        // 不额外调用 pwd 的情况下感知后续命令的起始位置。
        if let Some(cwd) = &final_cwd {
            let moved = effective_prev.as_deref() != Some(Path::new(cwd));
            if moved {
                text.push_str(&format!("\n[cwd: {cwd}]"));
            }
        }
        if text.len() > OUTPUT_LIMIT {
            // v1.7 截断落盘:已捕获的完整输出(stdout/stderr 分段)写临时
            // 文件,供模型用 read 工具取回;写失败维持纯截断(尽力而为)。
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let spill_path = std::env::temp_dir()
                .join(format!("reflect-bash-{}-{nanos}.log", std::process::id()));
            let mut spill = cleaned_out.clone();
            if !cleaned_err.is_empty() {
                spill.extend_from_slice(b"\n\n--- stderr ---\n");
                spill.extend_from_slice(&cleaned_err);
            }
            let spilled = std::fs::write(&spill_path, &spill).is_ok();
            // 0.8:`String::truncate` 在非 char 边界会 panic(一个汉字 3 字节,
            // 恰好切在第二 / 第三字节就炸)。改用协议层提供的安全截断。
            reflect_protocol::truncate_string_in_place(&mut text, OUTPUT_LIMIT);
            text.push_str("\n... [truncated]");
            if spilled {
                text.push_str(&format!("\n[完整输出已落盘: {}]", spill_path.display()));
            }
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
                "cwd": final_cwd.clone().or_else(|| effective_prev.map(|p| p.to_string_lossy().into_owned())),
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
        let t = BashTool::default();
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

    // ── v1.4 A3:输出流式增量 ─────────────────────────────────────

    /// 有进度回调时:bash 逐行上报增量(stdout + stderr 各自成段),
    /// 且最终 ToolOutput 的完整文本语义与无回调路径一致。
    /// 注意沙箱 env 串行化(见 SANDBOX_ENV_LOCK):显式关沙箱跑纯 echo,
    /// 不与其它测试竞争 env。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bash_reports_line_deltas_via_progress_sink() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
        }
        let deltas: std::sync::Arc<std::sync::Mutex<Vec<(bool, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_deltas = deltas.clone();
        let mut ctx = ToolContext::for_workspace("/tmp");
        ctx.progress = Some(crate::tool::ProgressSink(std::sync::Arc::new(
            move |is_stderr, delta| {
                sink_deltas
                    .lock()
                    .unwrap()
                    .push((is_stderr, delta.to_string()));
            },
        )));
        let out = BashTool::default()
            .execute(
                ctx,
                serde_json::json!({"cmd": "echo line1; echo err1 1>&2; echo line2"}),
            )
            .await;
        match prior_on {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
        }
        match prior_strict {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
        }
        let out = out.unwrap();
        let got = deltas.lock().unwrap().clone();
        let stdout_lines: Vec<&str> = got
            .iter()
            .filter(|(err, _)| !err)
            .map(|(_, d)| d.as_str())
            .collect();
        let stderr_lines: Vec<&str> = got
            .iter()
            .filter(|(err, _)| *err)
            .map(|(_, d)| d.as_str())
            .collect();
        assert_eq!(
            stdout_lines,
            vec!["line1\n", "line2\n"],
            "stdout 应按行逐段上报"
        );
        assert_eq!(stderr_lines, vec!["err1\n"], "stderr 应单独成段且标记");
        // 最终完整输出仍包含全部内容(增量只是预览)。
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("line1") && text.contains("line2") && text.contains("err1"));
    }

    /// 无进度回调(默认):行为与历史路径一致,零增量上报。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bash_without_sink_no_deltas_but_full_output() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
        }
        let out = BashTool::default()
            .execute(
                ToolContext::for_workspace("/tmp"),
                serde_json::json!({"cmd": "echo plain-path"}),
            )
            .await;
        match prior_on {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
        }
        match prior_strict {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
        }
        let out = out.unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("plain-path"));
    }

    /// review 回归:输出超过 OUTPUT_LIMIT(100KB)的命令必须正常完成
    /// 并带 0 退出码 —— 修复前捕获满后停止读管道,子进程写满 pipe 阻塞,
    /// `wait()` 挂死直到外层 timeout 误杀(表现为「大输出必然超时」)。
    /// 限时 8s:修复后毫秒级完成;若回归挂死,timeout(30s 默认)远超
    /// 本断言,必然失败。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn oversized_output_completes_without_timeout_kill() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
        }
        let started = std::time::Instant::now();
        let out = BashTool::default()
            .execute(
                ToolContext::for_workspace("/tmp"),
                serde_json::json!({"cmd": "seq 1 30000", "timeout_ms": 30000}),
            )
            .await;
        match prior_on {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
        }
        match prior_strict {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
        }
        let out = out.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "大输出命令应在 8s 内完成,实际 {:?}(疑似管道排空回归)",
            started.elapsed()
        );
        assert_eq!(out.metadata.get("exit_code"), Some(&serde_json::json!(0)));
    }

    #[tokio::test]
    async fn bash_captures_exit_code_and_marks_failure() {
        // M3:成功命令 exit_code=0 + is_error=false;失败命令 exit_code=非0 + is_error=true。
        // metadata.exit_code 应被填充,供 TUI reducer 渲染 ExecResult 单元格。
        let t = BashTool::default();
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

    /// 启用流式输出后,内部状态行仍须用于还原失败退出码,且不能泄露给消费者。
    #[tokio::test]
    async fn progress_sink_preserves_failure_exit_code() {
        let deltas = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured = deltas.clone();
        let mut ctx = ToolContext::for_workspace("/tmp");
        ctx.os_sandbox = Some(false);
        ctx.progress = Some(crate::tool::ProgressSink(std::sync::Arc::new(
            move |_, delta| {
                captured.lock().unwrap().push_str(delta);
            },
        )));
        let output = BashTool::default()
            .execute(ctx, serde_json::json!({"cmd": "printf failed >&2; false"}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(output.metadata["exit_code"], 1);
        let deltas = deltas.lock().unwrap();
        assert!(deltas.contains("failed"));
        assert!(!deltas.contains(SENTINEL_PREFIX));
    }

    /// 两条流都超过捕获上限时,状态行可能丢失,进程退出码仍须保留失败。
    #[tokio::test]
    async fn truncated_streams_preserve_failure_exit_code() {
        let mut ctx = ToolContext::for_workspace("/tmp");
        ctx.os_sandbox = Some(false);
        ctx.progress = Some(crate::tool::ProgressSink(std::sync::Arc::new(|_, _| {})));
        let bytes = OUTPUT_LIMIT + 1024;
        let output = BashTool::default().execute(ctx, serde_json::json!({
            "cmd": format!("head -c {bytes} /dev/zero; head -c {bytes} /dev/zero >&2; false")
        })).await.unwrap();
        assert!(output.is_error);
        assert_eq!(output.metadata["exit_code"], 1);
        assert_eq!(output.metadata["stdout_bytes"], OUTPUT_LIMIT);
        assert_eq!(output.metadata["stderr_bytes"], OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn rejects_missing_cmd() {
        let t = BashTool::default();
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
        let t = BashTool::default();
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
        let t = BashTool::default();
        let err = t
            .execute(ToolContext::default(), serde_json::json!({"cmd": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_null_args() {
        // 模拟流式 JSON 解析失败退化为 Value::Null 的场景。
        let t = BashTool::default();
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
        let t = BashTool::default();
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
        let t = BashTool::default();
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
        let t = BashTool::default();
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
        let t = BashTool::default();
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

    // ── v1.7 持久 shell 状态(cwd 跨调用保留 + sentinel 剥除 + 截断落盘) ──

    /// 提取 sentinel 行的纯函数测试:rc 与含空格的 cwd 都能解析,输出
    /// 剥除后保留 `printf` 前置空行之前的原始内容。
    #[test]
    fn extract_state_parses_rc_and_cwd_with_spaces() {
        let out = b"abc\n\n__REFLECT_STATE__ rc=2 cwd=/tmp/a b/c\n";
        let (state, cleaned) = extract_shell_state(out);
        let (rc, cwd) = state.expect("sentinel 应可解析");
        assert_eq!(rc, 2);
        assert_eq!(cwd, "/tmp/a b/c");
        assert_eq!(cleaned, b"abc\n");
    }

    /// 无 sentinel(进程被杀 / `exit` 提前退出)→ 输出原样返回。
    #[test]
    fn extract_state_absent_leaves_output_untouched() {
        let (state, cleaned) = extract_shell_state(b"plain output");
        assert!(state.is_none());
        assert_eq!(cleaned, b"plain output");
    }

    /// 用户输出里伪造 sentinel 行:取**最后一行**命中,真实 sentinel 被
    /// 剥除,伪造行保留在输出里(不影响 rc 解析)。
    #[test]
    fn extract_state_takes_last_sentinel_line() {
        let out = b"echo __REFLECT_STATE__ rc=99 cwd=/fake\n__REFLECT_STATE__ rc=0 cwd=/real\n";
        let (state, cleaned) = extract_shell_state(out);
        let (rc, cwd) = state.expect("应解析到真实 sentinel");
        assert_eq!(rc, 0);
        assert_eq!(cwd, "/real");
        assert!(
            String::from_utf8_lossy(&cleaned).contains("rc=99"),
            "伪造行应保留: {}",
            String::from_utf8_lossy(&cleaned)
        );
    }

    /// cwd 跨调用持久:第一次 `cd` 进子目录(metadata.cwd 更新 + 输出带
    /// `[cwd: ...]` 提示),第二次 `pwd` 直接落在该目录且不再重复提示。
    #[tokio::test]
    async fn cwd_persists_across_calls() {
        let dir = std::env::temp_dir().join(format!("reflect-bash-cwd-{}-p", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::default();
        let ctx = ToolContext::for_workspace("/tmp");
        let out1 = tool
            .execute(
                ctx.clone(),
                serde_json::json!({"cmd": format!("cd -- {}", shell_quote(&dir))}),
            )
            .await
            .unwrap();
        assert!(!out1.is_error, "cd 应成功: {:?}", out1.content[0]);
        assert_eq!(
            out1.metadata.get("cwd"),
            Some(&serde_json::json!(dir.to_string_lossy().into_owned())),
            "metadata.cwd 应反映新目录"
        );
        let text1 = match &out1.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text1.contains(&format!("[cwd: {}]", dir.display())),
            "移动目录后应追加提示: {text1:?}"
        );

        let out2 = tool
            .execute(ctx, serde_json::json!({"cmd": "pwd"}))
            .await
            .unwrap();
        let text2 = match &out2.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text2.contains(dir.to_string_lossy().as_ref()),
            "pwd 应落在持久目录: {text2:?}"
        );
        assert!(
            !text2.contains("[cwd:"),
            "目录未变时不应重复提示: {text2:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sentinel 行剥除:最终输出不泄漏协议行,且无尾部换行的输出(如
    /// `printf hello`)保持字节语义不变。
    #[tokio::test]
    async fn sentinel_line_stripped_and_byte_semantics_kept() {
        let t = BashTool::default();
        let out = t
            .execute(
                ToolContext::for_workspace("/tmp"),
                serde_json::json!({"cmd": "printf hello"}),
            )
            .await
            .unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            !text.contains("__REFLECT_STATE__"),
            "sentinel 不应出现在输出中: {text:?}"
        );
        assert_eq!(text, "hello", "无尾部换行的输出应原样保留");
    }

    /// `exit N` 提前终止 shell → 无 sentinel,进程退出码兜底(= N),
    /// 持久 cwd 保持旧值不变。
    #[tokio::test]
    async fn early_exit_uses_process_exit_code() {
        let t = BashTool::default();
        let out = t
            .execute(
                ToolContext::for_workspace("/tmp"),
                serde_json::json!({"cmd": "exit 7"}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata.get("exit_code"), Some(&serde_json::json!(7)));
        assert!(out.is_error, "非零退出码应标记 error");
    }

    /// 持久目录被删 → 状态清除、回落 workspace 根,命令正常执行不报错。
    #[tokio::test]
    async fn stale_cwd_falls_back_to_workspace() {
        let dir = std::env::temp_dir().join(format!("reflect-bash-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tool = BashTool::default();
        let ctx = ToolContext::for_workspace("/tmp");
        tool.execute(
            ctx.clone(),
            serde_json::json!({"cmd": format!("cd -- {}", shell_quote(&dir))}),
        )
        .await
        .unwrap();
        std::fs::remove_dir(&dir).unwrap();
        let out = tool
            .execute(ctx, serde_json::json!({"cmd": "pwd"}))
            .await
            .unwrap();
        assert!(!out.is_error, "回落路径应正常执行: {:?}", out.content[0]);
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("tmp"), "应回落到 workspace 根: {text:?}");
    }

    /// 输出超过 OUTPUT_LIMIT → 截断 + 完整捕获输出落盘,输出末尾给出路径。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn oversized_output_spilled_to_disk() {
        let _g = SANDBOX_ENV_LOCK.lock().unwrap();
        let prior_on = std::env::var("REFLECT_SANDBOX_OS_LEVEL").ok();
        let prior_strict = std::env::var("REFLECT_SANDBOX_STRICT").ok();
        unsafe {
            std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", "0");
            std::env::set_var("REFLECT_SANDBOX_STRICT", "0");
        }
        let out = BashTool::default()
            .execute(
                ToolContext::for_workspace("/tmp"),
                // stdout 与 stderr 双流超限:确保合并文本必然超过
                // OUTPUT_LIMIT(stdout 单流恰好在 cap 上不会触发截断)。
                serde_json::json!({"cmd": "seq 1 30000; seq 1 1000 1>&2", "timeout_ms": 30000}),
            )
            .await;
        match prior_on {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_OS_LEVEL", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_OS_LEVEL") },
        }
        match prior_strict {
            Some(p) => unsafe { std::env::set_var("REFLECT_SANDBOX_STRICT", p) },
            None => unsafe { std::env::remove_var("REFLECT_SANDBOX_STRICT") },
        }
        let out = out.unwrap();
        let text = match &out.content[0] {
            reflect_protocol::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("... [truncated]"), "应截断");
        let marker = "[完整输出已落盘: ";
        let path_str = text
            .split(marker)
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("截断输出应带落盘路径");
        let bytes = std::fs::read(path_str).expect("落盘文件应存在");
        assert!(
            bytes.len() > OUTPUT_LIMIT,
            "落盘应包含完整捕获输出(>{} bytes),实际 {}",
            OUTPUT_LIMIT,
            bytes.len()
        );
        assert!(
            !String::from_utf8_lossy(&bytes).contains("__REFLECT_STATE__"),
            "落盘内容不应包含 sentinel 行"
        );
        let _ = std::fs::remove_file(path_str);
    }
}
