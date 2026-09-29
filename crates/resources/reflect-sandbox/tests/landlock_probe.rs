//! Landlock 逐阶段探针 —— 仅用于 CI 环境(runner)诊断沙箱行为。
//!
//! 在 Linux runner 上直接输出每一步的返回值 / errno,配合
//! `cargo test -p reflect-sandbox --test landlock_probe -- --nocapture`
//! 使用。失败断言极松:只要求进程活着跑完全程,诊断信息全靠打印。

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::path::PathBuf;

const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
const SYS_LANDLOCK_ADD_RULE: i64 = 445;
const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;
const PR_SET_NO_NEW_PRIVS: i32 = 38;

unsafe extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
    fn __errno_location() -> *mut i32;
}

fn errno() -> i32 {
    unsafe { *__errno_location() }
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[tokio::test]
async fn probe_landlock_stages() {
    eprintln!("== landlock probe ==");
    eprintln!(
        "uname/runner info skipped; cwd = {:?}",
        std::env::current_dir()
    );

    // 1. prctl NNP。
    let nnp = unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    eprintln!(
        "1. prctl(NNP) = {nnp} (errno={})",
        if nnp != 0 { errno() } else { 0 }
    );

    // 2. create ruleset —— 两种 handled 位集都试。
    for (name, bits) in [("write-only 0xffe", 0xffeu64), ("all 0x1fff", 0x1fffu64)] {
        let attr = RulesetAttr {
            handled_access_fs: bits,
            handled_access_net: 0,
        };
        let fd = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<RulesetAttr>(),
                0,
            )
        };
        eprintln!(
            "2. create_ruleset({name}) = {fd} (errno={})",
            if fd < 0 { errno() } else { 0 }
        );
        if fd < 0 {
            continue;
        }
        let fd = fd as i32;

        // 3. open + add rule 对 /tmp 与 cwd。
        for dir in [
            "/tmp".to_string(),
            std::env::current_dir().unwrap().display().to_string(),
        ] {
            let c = CString::new(dir.clone()).unwrap();
            let pfd = unsafe { open(c.as_ptr(), 0o200000) };
            eprintln!(
                "3. open(O_PATH) {dir} = {pfd} (errno={})",
                if pfd < 0 { errno() } else { 0 }
            );
            if pfd < 0 {
                continue;
            }
            let pb = PathBeneathAttr {
                allowed_access: bits,
                parent_fd: pfd,
            };
            let r = unsafe { syscall(SYS_LANDLOCK_ADD_RULE, fd as i64, 1, &pb as *const _, 0) };
            eprintln!(
                "4. add_rule({dir}) = {r} (errno={})",
                if r < 0 { errno() } else { 0 }
            );
            unsafe { close(pfd) };
        }

        // 4. restrict self。
        let r = unsafe { syscall(SYS_LANDLOCK_RESTRICT_SELF, fd as i64, 0) };
        eprintln!(
            "5. restrict_self({name}) = {r} (errno={})",
            if r < 0 { errno() } else { 0 }
        );
        unsafe { close(fd) };
        if r == 0 {
            // 5. restrict 后做一组分解实验,把 EACCES 的来源定位到具体变量。
            let mut report = String::from("post-restrict 分解实验:\n");

            // a. 读未 handled 的路径(应不受限)。
            match std::fs::read_to_string("/etc/hostname") {
                Ok(_) => report.push_str("  a. read /etc/hostname = ok\n"),
                Err(e) => report.push_str(&format!("  a. read /etc/hostname = ERR {e}\n")),
            }
            // b. metadata /bin/sh。
            match std::fs::metadata("/bin/sh") {
                Ok(m) => report.push_str(&format!("  b. metadata /bin/sh = ok len={}\n", m.len())),
                Err(e) => report.push_str(&format!("  b. metadata /bin/sh = ERR {e}\n")),
            }
            // c. std::process 同步 spawn,保留全部 env。
            match std::process::Command::new("/bin/true").status() {
                Ok(s) => report.push_str(&format!("  c. std spawn /bin/true (env 保留) = {s:?}\n")),
                Err(e) => report.push_str(&format!("  c. std spawn /bin/true = ERR {e}\n")),
            }
            // d. std spawn env_clear。
            match std::process::Command::new("/bin/true").env_clear().status() {
                Ok(s) => {
                    report.push_str(&format!("  d. std spawn /bin/true (env_clear) = {s:?}\n"))
                }
                Err(e) => {
                    report.push_str(&format!("  d. std spawn /bin/true (env_clear) = ERR {e}\n"))
                }
            }
            // e. tokio spawn /bin/sh -c,保留 env。
            match tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("echo probe-ok")
                .output()
                .await
            {
                Ok(o) => report.push_str(&format!(
                    "  e. tokio spawn /bin/sh (env 保留) = {:?} out={}\n",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).trim()
                )),
                Err(e) => report.push_str(&format!("  e. tokio spawn /bin/sh = ERR {e}\n")),
            }
            // f. tokio spawn /bin/sh -c,env_clear(复现 bash 工具路径)。
            match tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("echo probe-ok")
                .env_clear()
                .output()
                .await
            {
                Ok(o) => report.push_str(&format!(
                    "  f. tokio spawn /bin/sh (env_clear) = {:?} out={}\n",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).trim()
                )),
                Err(e) => {
                    report.push_str(&format!("  f. tokio spawn /bin/sh (env_clear) = ERR {e}\n"))
                }
            }

            // libtest 连 stderr 一起捕获(pass 时吞掉)—— 用 panic 回放
            // 强制显示全部诊断信息。
            panic!("landlock probe restrict({name}) 成功后的分解实验报告:\n{report}");
        }
    }
    eprintln!("== probe done(未走到 restrict 成功分支)==");
    // io 依赖占位,避免 unused。
    let _ = io::IoSlice::new(&[]);
    let _: PathBuf = Default::default();
}
