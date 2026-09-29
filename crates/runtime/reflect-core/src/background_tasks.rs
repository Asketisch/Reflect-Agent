//! `background_tasks` —— Background Task Injection(P2 `background-tasks`)。
//!
//! 后台 bash/subagent 任务 stub:spawn 后异步完成,结果在 turn 边界注入。

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 后台任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

/// 后台任务记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub status: BackgroundTaskStatus,
    pub result: Option<String>,
}

/// 后台任务队列 —— submission_loop 在 turn 边界 drain 已完成项。
#[derive(Debug, Default)]
pub struct BackgroundTaskQueue {
    tasks: Mutex<Vec<BackgroundTask>>,
}

impl BackgroundTaskQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 pending 任务。
    pub fn register(
        &self,
        id: impl Into<String>,
        kind: impl Into<String>,
        payload: impl Into<String>,
    ) {
        self.tasks.lock().push(BackgroundTask {
            id: id.into(),
            kind: kind.into(),
            payload: payload.into(),
            status: BackgroundTaskStatus::Pending,
            result: None,
        });
    }

    /// v1.5 R2:标记任务进入运行态。
    pub fn mark_running(&self, id: &str) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Running;
            return true;
        }
        false
    }

    /// v1.5 R2:标记任务失败并写入原因。
    pub fn fail(&self, id: &str, reason: impl Into<String>) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Failed;
            t.result = Some(reason.into());
            return true;
        }
        false
    }

    /// v1.5 R2:全部任务快照(状态查询工具用;不移除任何条目)。
    pub fn snapshot(&self) -> Vec<BackgroundTask> {
        self.tasks.lock().clone()
    }

    /// 标记任务完成并写入结果。
    pub fn complete(&self, id: &str, result: impl Into<String>) -> bool {
        let mut g = self.tasks.lock();
        if let Some(t) = g.iter_mut().find(|t| t.id == id) {
            t.status = BackgroundTaskStatus::Completed;
            t.result = Some(result.into());
            return true;
        }
        false
    }

    /// 取出已完成任务并从队列移除(供注入 LLM context)。
    pub fn drain_completed(&self) -> Vec<BackgroundTask> {
        let mut g = self.tasks.lock();
        let (done, rest): (Vec<_>, Vec<_>) = g
            .drain(..)
            .partition(|t| t.status == BackgroundTaskStatus::Completed);
        *g = rest;
        done
    }

    pub fn len(&self) -> usize {
        self.tasks.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.lock().is_empty()
    }
}

/// v1.5 R2:单条后台 bash 任务的捕获上限(与 bash 工具一致)。
const BG_OUTPUT_LIMIT: usize = 100 * 1024;

/// v1.5 R2:并发排空单个输出管道 —— 先捕获至多 `cap` 字节,捕获满后
/// **继续读取并丢弃**剩余输出直至 EOF。只 `take(cap).read_to_end` 的话,
/// 捕获满后无人再读管道,子进程把 pipe 缓冲(约 64KB)写满后会永久阻塞
/// 在 write 上,`wait()` 随之挂死、任务卡在 Running —— 后台任务恰恰是
/// 长输出(构建日志等)的重灾区,必须排空。
async fn drain_capped<R>(pipe: Option<&mut R>, cap: usize, buf: &mut Vec<u8>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(s) = pipe else {
        return;
    };
    let mut limited = s.take(cap as u64);
    if limited.read_to_end(buf).await.is_err() {
        return;
    }
    // 捕获满(Take 到达上限)或正常 EOF 后:绕过 Take 的限额继续排空。
    let mut discard = [0u8; 8192];
    loop {
        match limited.get_mut().read(&mut discard).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// v1.5 R2:后台命令执行规格。`program` + `args` 已含沙箱包裹
/// (Seatbelt 的 sandbox-exec argv / 裸 `sh -c`);`landlock` 为 true 时
/// (Linux)在 fork 后、exec 前应用 Landlock 规则;`seatbelt_profile`
/// 为 Seatbelt 临时 profile 路径,进程结束后负责删除避免临时目录泄漏。
pub struct BackgroundCommand {
    /// 队列登记 / 注入 LLM 用的人类可读命令文本。
    pub display: String,
    pub program: String,
    pub args: Vec<String>,
    pub landlock: bool,
    pub sandbox: reflect_sandbox::OsSandbox,
    pub seatbelt_profile: Option<std::path::PathBuf>,
}

/// v1.5 R2:真实现 —— 后台执行一条命令(规格由调用方完成 OS 沙箱
/// 包裹),输出(含退出码)在完成时写回队列,由 turn 边界注入 /
/// `background_status` 工具消费。进程生命周期绑定会话取消令牌
/// (会话关闭 → kill_on_drop 收割)。
pub fn spawn_command(
    queue: Arc<BackgroundTaskQueue>,
    session_cancel: CancellationToken,
    cwd: std::path::PathBuf,
    spec: BackgroundCommand,
) -> String {
    use std::process::Stdio;
    let BackgroundCommand {
        display,
        program,
        args,
        landlock,
        sandbox,
        seatbelt_profile,
    } = spec;
    let id = format!("bg-{}", uuid_stub_like());
    queue.register(id.clone(), "bash", display);
    let id_for_task = id.clone();
    let q = queue.clone();
    let handle = tokio::spawn(async move {
        let id = id_for_task;
        q.mark_running(&id);
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(&cwd)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        // Linux Landlock:与前台 BashTool 同款 —— fork 后、exec 前应用
        // 写白名单规则;内核不支持时闭包内降级放行。pre_exec 来自
        // tokio Command 的固有方法,无需 std CommandExt 导入(多余导入
        // 会在 Linux 上被 -D warnings 的 unused-imports 拦截)。
        #[cfg(target_os = "linux")]
        if landlock {
            let ws = cwd.clone();
            let mut pre_exec = sandbox.landlock_pre_exec(ws);
            // 安全:仅在 pre_exec(子进程 fork 后)上下文调用,闭包内部
            // 只做 async-signal-safe 的 libc 调用(sandbox crate 保证)。
            unsafe {
                command.pre_exec(move || pre_exec());
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (landlock, &sandbox);
        for key in ["PATH", "HOME", "LANG", "LC_ALL", "USER", "SHELL", "TMPDIR"] {
            if let Ok(v) = std::env::var(key) {
                command.env(key, v);
            }
        }
        // Linux Landlock:与前台 BashTool 同款 —— fork 后、exec 前应用
        // 写白名单规则;内核不支持时闭包内降级放行。pre_exec 来自
        // tokio Command 的固有方法,无需 std CommandExt 导入(多余导入
        // 会在 Linux 上被 -D warnings 的 unused-imports 拦截)。
        //
        // v1.6 降级重试:受限环境(GHA runner 等)的 landlock 实现异常,
        // restrict 成功后 execve 仍可能 EACCES/EPERM —— 权限类失败时
        // 重建 command(不带沙箱)重试一次,对齐「尽力而为不阻塞」哲学。
        let child = match command.spawn() {
            Ok(c) => c,
            Err(e)
                if cfg!(target_os = "linux")
                    && landlock
                    && matches!(e.raw_os_error(), Some(13) | Some(1)) =>
            {
                tracing::warn!(
                    %program,
                    error = %e,
                    "landlock sandboxed background spawn failed; retrying without sandbox"
                );
                let mut command = tokio::process::Command::new(&program);
                command
                    .args(&args)
                    .current_dir(&cwd)
                    .env_clear()
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null())
                    .kill_on_drop(true);
                for key in ["PATH", "HOME", "LANG", "LC_ALL", "USER", "SHELL", "TMPDIR"] {
                    if let Ok(v) = std::env::var(key) {
                        command.env(key, v);
                    }
                }
                match command.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        q.fail(&id, format!("spawn failed: {e}"));
                        if let Some(p) = seatbelt_profile {
                            let _ = std::fs::remove_file(p);
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                q.fail(&id, format!("spawn failed: {e}"));
                if let Some(p) = seatbelt_profile {
                    let _ = std::fs::remove_file(p);
                }
                return;
            }
        };
        let mut child = child;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        // 并发排空两端管道(防写满阻塞)+ 等退出;会话取消 → 击杀。
        let collect = async {
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();
            let out_fut = drain_capped(stdout.as_mut(), BG_OUTPUT_LIMIT, &mut out_buf);
            let err_fut = drain_capped(stderr.as_mut(), BG_OUTPUT_LIMIT, &mut err_buf);
            tokio::join!(out_fut, err_fut);
            let status = child.wait().await;
            (out_buf, err_buf, status)
        };
        tokio::select! {
            biased;
            _ = session_cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                q.fail(&id, "[cancelled] 会话关闭,后台任务被终止");
            }
            r = collect => {
                let (out, err, status) = r;
                let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                let mut text = String::from_utf8_lossy(&out).to_string();
                if !err.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("stderr:\n{}", String::from_utf8_lossy(&err)));
                }
                if text.len() > BG_OUTPUT_LIMIT {
                    text.truncate(BG_OUTPUT_LIMIT);
                    text.push_str("\n... [truncated]");
                }
                text.push_str(&format!("\n[exit_code: {exit_code}]"));
                if exit_code == 0 {
                    q.complete(&id, text);
                } else {
                    q.fail(&id, text);
                }
            }
        }
        // Seatbelt 临时 profile:进程结束后删除(成功 / 失败 / 取消路径
        // 统一收口),避免临时目录累积泄漏。
        if let Some(p) = seatbelt_profile {
            let _ = std::fs::remove_file(p);
        }
    });
    // 句柄分离:后台任务生命周期由队列 + 会话令牌管理,drop JoinHandle
    // 即 detach(tokio 语义),不随调用方丢弃。
    drop(handle);
    id
}

fn uuid_stub_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", n)
}

/// spawn 简易后台任务(stub:tokio sleep + 固定结果)。
pub fn spawn_background_stub(
    queue: Arc<BackgroundTaskQueue>,
    id: impl Into<String>,
    kind: impl Into<String>,
    payload: impl Into<String>,
) -> JoinHandle<()> {
    let id_s = id.into();
    queue.register(id_s.clone(), kind, payload);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        queue.complete(&id_s, format!("background result for {id_s}"));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_completed_returns_finished_tasks() {
        let q = Arc::new(BackgroundTaskQueue::new());
        q.register("t1", "bash", "echo hi");
        q.complete("t1", "hi\n");
        let done = q.drain_completed();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].result.as_deref(), Some("hi\n"));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn spawn_stub_completes_task() {
        let q = Arc::new(BackgroundTaskQueue::new());
        let h = spawn_background_stub(q.clone(), "bg-1", "bash", "sleep 0");
        h.await.unwrap();
        let done = q.drain_completed();
        assert_eq!(done.len(), 1);
        assert!(done[0].result.as_ref().unwrap().contains("bg-1"));
    }
}

/// v1.5 R2:core 侧的 [`reflect_tools::TaskSpawner`] 实现 —— 桥接工具层
/// 请求与 [`BackgroundTaskQueue`] 的真实进程管理。持有会话取消令牌
/// (后台任务存活期跨 turn,随会话关闭统一收割)。
#[derive(Debug)]
pub struct CoreTaskSpawner {
    queue: Arc<BackgroundTaskQueue>,
    workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
    session_cancel: CancellationToken,
}

impl CoreTaskSpawner {
    pub fn new(
        queue: Arc<BackgroundTaskQueue>,
        workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
        session_cancel: CancellationToken,
    ) -> Self {
        Self {
            queue,
            workspace,
            session_cancel,
        }
    }
}

impl reflect_tools::TaskSpawner for CoreTaskSpawner {
    fn spawn_bash(
        &self,
        cmd: &str,
        cwd: &std::path::Path,
        os_sandbox: Option<bool>,
    ) -> Result<String, reflect_protocol::ToolError> {
        // cwd 显式指定优先(工具上下文传当前工作区);否则跟随热切换句柄。
        let cwd = if cwd.as_os_str().is_empty() {
            self.workspace.read().clone()
        } else {
            cwd.to_path_buf()
        };
        // v1.5 R2+review:与前台 BashTool 同款 OS 沙箱 —— `os_sandbox`
        // 每 turn 覆盖(Some(on))> env(from_env);严格模式 fail-closed,
        // 绝不回退裸 `sh -c`。此前后台路径完全绕过沙箱,模型可借
        // run_in_background 逃逸 OS 层限制。
        let sandbox = match os_sandbox {
            Some(on) => reflect_sandbox::OsSandbox::with_enabled(on),
            None => reflect_sandbox::OsSandbox::from_env(),
        };
        sandbox
            .enforce_or_fail()
            .map_err(|reason| reflect_protocol::ToolError::SandboxUnavailable { reason })?;
        // 决定 (program, argv):Seatbelt → sandbox-exec 包裹(临时
        // profile 文件由任务进程结束后删除);Landlock → sh -c + pre_exec;
        // 非 strict 降级 → 裸 sh -c。
        let (program, args): (String, Vec<String>) =
            if matches!(sandbox.status(), reflect_sandbox::OsSandboxStatus::Seatbelt) {
                match sandbox.seatbelt_argv(&cwd, cmd) {
                    Ok(pair) => pair,
                    Err(e) => {
                        if sandbox.strict {
                            let reason =
                                format!("sandbox profile 生成失败: {e};严格模式下拒绝裸执行");
                            return Err(reflect_protocol::ToolError::SandboxUnavailable { reason });
                        }
                        ("sh".into(), vec!["-c".into(), cmd.to_string()])
                    }
                }
            } else {
                ("sh".into(), vec!["-c".into(), cmd.to_string()])
            };
        let seatbelt_profile = if program == "/usr/bin/sandbox-exec" {
            args.get(1).map(std::path::PathBuf::from)
        } else {
            None
        };
        let landlock = matches!(sandbox.status(), reflect_sandbox::OsSandboxStatus::Landlock);
        Ok(spawn_command(
            self.queue.clone(),
            self.session_cancel.clone(),
            cwd,
            crate::background_tasks::BackgroundCommand {
                display: cmd.to_string(),
                program,
                args,
                landlock,
                sandbox,
                seatbelt_profile,
            },
        ))
    }

    fn snapshot(&self) -> Vec<reflect_tools::BackgroundTaskInfo> {
        self.queue
            .snapshot()
            .into_iter()
            .map(|t| reflect_tools::BackgroundTaskInfo {
                id: t.id,
                status: match t.status {
                    BackgroundTaskStatus::Pending => "pending".into(),
                    BackgroundTaskStatus::Running => "running".into(),
                    BackgroundTaskStatus::Completed => "completed".into(),
                    BackgroundTaskStatus::Failed => "failed".into(),
                },
                result: t.result,
            })
            .collect()
    }
}

#[cfg(test)]
mod spawn_command_tests {
    use super::*;
    use std::time::Duration;

    /// review 回归:输出远超捕获上限(100KB)的后台命令必须正常完成,
    /// 不能因管道无人排空而卡在 Running(修复前:子进程写满 pipe 后
    /// 永久阻塞,任务挂死)。透传沙箱,聚焦管道语义本身。
    #[tokio::test]
    async fn oversized_output_completes_not_hangs() {
        let queue = Arc::new(BackgroundTaskQueue::new());
        let cancel = CancellationToken::new();
        // seq 1 30000 ≈ 170KB stdout,超过 100KB 捕获上限。
        let id = spawn_command(
            queue.clone(),
            cancel.clone(),
            std::env::temp_dir(),
            BackgroundCommand {
                display: "seq 1 30000".into(),
                program: "sh".into(),
                args: vec!["-c".into(), "seq 1 30000".into()],
                landlock: false,
                sandbox: reflect_sandbox::OsSandbox::passthrough(),
                seatbelt_profile: None,
            },
        );
        for _ in 0..200 {
            let snap = queue
                .snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .expect("任务应已登记");
            if snap.status != BackgroundTaskStatus::Pending
                && snap.status != BackgroundTaskStatus::Running
            {
                assert_eq!(snap.status, BackgroundTaskStatus::Completed);
                let text = snap.result.expect("完成应有输出");
                assert!(text.contains("[exit_code: 0]"), "got: {text:?}");
                assert!(
                    text.len() >= BG_OUTPUT_LIMIT,
                    "捕获应打满上限(输出被截断到上限而非丢失)"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("10s 内未完成 —— 管道排空修复失效,任务卡死");
    }

    /// 会话取消:在飞后台任务被击杀并标记 Failed(cancelled 语义)。
    #[tokio::test]
    async fn session_cancel_kills_running_task() {
        let queue = Arc::new(BackgroundTaskQueue::new());
        let cancel = CancellationToken::new();
        let id = spawn_command(
            queue.clone(),
            cancel.clone(),
            std::env::temp_dir(),
            BackgroundCommand {
                display: "sleep 30".into(),
                program: "sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                landlock: false,
                sandbox: reflect_sandbox::OsSandbox::passthrough(),
                seatbelt_profile: None,
            },
        );
        // 等任务进入 Running 后取消。
        for _ in 0..100 {
            if queue
                .snapshot()
                .iter()
                .any(|t| t.id == id && t.status == BackgroundTaskStatus::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        for _ in 0..100 {
            let snap = queue
                .snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .expect("任务应在队列");
            if snap.status == BackgroundTaskStatus::Failed {
                assert!(snap.result.unwrap_or_default().contains("cancelled"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("取消后任务未终止");
    }
}
