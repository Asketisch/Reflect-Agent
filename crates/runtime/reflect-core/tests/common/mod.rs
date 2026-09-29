//! 集成测试共享工具。
//!
//! `exec_capable`:Linux 上用**隔离子进程**(重跑当前测试二进制的
//! `landlock_probe_child` 入口)探测「landlock restrict 后仍能 exec」。
//!
//! 背景:GHA ubuntu runner 的 landlock 实现异常 —— restrict(即使
//! handled 仅写类)成功后 read / execve 一律 EACCES,违背内核
//! 『未 handled 不受限』语义(CI 探针 reflect-sandbox/tests/landlock_probe
//! 实测);真实 Linux 桌面/服务器不受影响。能力缺失时依赖真实子进程的
//! 测试(bash 全链路 / MCP 子进程等)应跳过 —— 环境限制,非回归。

/// 子进程入口:prctl(NNP) + create(写类 handled)+ add(cwd)+ restrict,
/// 然后 spawn /bin/true。成功 exit 0;失败 exit 42。仅在父进程以
/// `LANDLOCK_PROBE_CHILD=1` env 触发时执行实际探测。
#[cfg(target_os = "linux")]
#[test]
fn landlock_probe_child() {
    if std::env::var("LANDLOCK_PROBE_CHILD").as_deref() != Ok("1") {
        // 常规测试跑选中时直接通过(探测仅由 exec_capable 子进程触发)。
        return;
    }
    if !reflect_sandbox::probe_landlock_exec() {
        std::process::exit(42);
    }
}

/// 当前进程环境是否支持「landlock restrict 后 exec」。非 Linux 恒 true。
pub fn exec_capable() -> bool {
    #[cfg(target_os = "linux")]
    {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return true, // 无法探测 → 不拦测试
        };
        match std::process::Command::new(exe)
            .args([
                "--exact",
                "landlock_probe_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LANDLOCK_PROBE_CHILD", "1")
            .status()
        {
            Ok(s) => s.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
