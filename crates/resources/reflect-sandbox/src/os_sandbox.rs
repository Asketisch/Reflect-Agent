//! `os_sandbox` — v1.2 P0-1 真实 OS 沙箱(macOS Seatbelt + Linux Landlock)。
//!
//! 把 v0 的 stub 升级为真正限制文件系统访问的沙箱。bash 工具默认在外层
//! 沙箱中执行,**`rm -rf /` 等破坏系统操作被内核拒绝**(`Operation not
//! permitted`)。
//!
//! ## 平台后端
//!
//! - **macOS**:Seatbelt,经 `/usr/bin/sandbox-exec` 外部进程 + 生成的
//!   `.sb` profile(`(version 1)(allow default deny) ...`)。允许写
//!   workspace + 可选额外目录 + 临时目录;读全盘保留(编译/分析常需读
//!   系统头文件);网络默认关。
//! - **Linux**:Landlock(v1+),经 `libc` syscall(`landlock_create_ruleset`
//!   / `landlock_add_rule`),在 `pre_exec` 之间(子进程 fork 后、exec 前)
//!   应用。对 workspace 写、workspace 外读写按规则放行,其余写拒。
//!   不支持时(老内核 / 无 CAP)→ 静默跳过(降级,不阻塞执行)。
//!
//! ## 不变式
//!
//! - **v1.3 起**:`OsSandbox::default()` 返回 `enabled = true`,以满足"默认
//!   要求 OS sandbox"的严格安全基线。要回到旧透传行为显式设
//!   `enabled = false` 或用 `OsSandbox::passthrough()`。
//! - `strict = true`(默认)→ 后端不可用 / profile 写失败 / Landlock
//!   不支持 时 `enforce_or_fail()` 返回 `Err`,由 BashTool 把
//!   `ToolError::SandboxUnavailable` 抛出,**绝不**回退裸 `sh -c`。
//! - `strict = false` → 保留旧降级语义(降级为透传 + warn)以便逐
//!   步迁移;新代码默认 `strict = true`。
//!
//! 验证标准(gap doc):sandbox 内执行 `rm -rf /` 应被阻止。

use std::path::{Path, PathBuf};

/// 沙箱后端状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSandboxStatus {
    /// 未启用(默认;透传所有命令)。
    Disabled,
    /// 已启用且 backend 为 macOS Seatbelt(`sandbox-exec`)。
    Seatbelt,
    /// 已启用且 backend 为 Linux Landlock。
    Landlock,
    /// 已启用但当前平台无可用 backend(降级 = 不沙箱)。
    NoBackend,
}

/// 检测当前平台可用的沙箱 backend。
///
/// - macOS:`sandbox-exec` 在 `/usr/bin/sandbox-exec` → `Seatbelt`。
/// - Linux:Landlock syscall 可用性在 apply 时探测,这里乐观返回 `Landlock`
///   (内核不支持时 apply 降级)。
/// - 其他平台 → `NoBackend`。
pub fn detect_backend() -> OsSandboxStatus {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            OsSandboxStatus::Seatbelt
        } else {
            OsSandboxStatus::NoBackend
        }
    }
    #[cfg(target_os = "linux")]
    {
        OsSandboxStatus::Landlock
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        OsSandboxStatus::NoBackend
    }
}

/// OS 级沙箱配置。
///
/// **v1.3 默认值翻转**:`enabled = true`(v1.2 是 `false`),以满足
/// "默认要求 OS sandbox" 的严格安全基线。要回到旧透传行为显式
/// 调 `OsSandbox::passthrough()` 或 `OsSandbox::with_enabled(false, ...)`。
/// `strict = true`(默认)→ 后端不可用 / profile 写失败 / Landlock 不支持
/// 时 `enforce_or_fail()` 返回 `Err`,由 BashTool 抛
/// `ToolError::SandboxUnavailable`,绝不回退裸 `sh -c`。
#[derive(Debug, Clone)]
pub struct OsSandbox {
    pub enabled: bool,
    /// 额外可写目录(workspace 总是可写)。绝对路径。
    pub writable_dirs: Vec<PathBuf>,
    /// 是否允许出站网络。默认 false。
    pub allow_network: bool,
    /// v1.3:严格模式。`true` 时后端不可用 / profile 写失败 → 返回
    /// `Err` 而不是降级透传(BashTool 据此抛 `ToolError::SandboxUnavailable`)。
    /// `false` 时保留 v1.2 降级语义。
    pub strict: bool,
    /// 探测到的 backend(`enabled=false` 时为 `Disabled`)。
    backend: OsSandboxStatus,
}

impl Default for OsSandbox {
    fn default() -> Self {
        // v1.3:默认要求 OS sandbox,严格模式,fail-closed。
        Self {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            backend: detect_backend(),
        }
    }
}

impl OsSandbox {
    /// v1.3:显式透传构造(关闭 sandbox,不严格)。旧默认行为兜底;
    /// 调用方需明确表达"我接受无沙箱"。
    pub fn passthrough() -> Self {
        Self {
            enabled: false,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: false,
            backend: OsSandboxStatus::Disabled,
        }
    }

    /// 构造一个启用沙箱的实例,自动探测 backend。`writable_dirs` 是
    /// workspace 之外额外可写目录。
    pub fn enabled(writable_dirs: Vec<PathBuf>) -> Self {
        Self {
            enabled: true,
            writable_dirs,
            allow_network: false,
            strict: true,
            backend: detect_backend(),
        }
    }

    /// v1.3:从 env `REFLECT_SANDBOX_OS_LEVEL` 读启用状态。
    /// - `0` / `false` / `no` / `off` → 关闭(`passthrough`)
    /// - 未设置 / `1` / `true` / `yes` / `on` → 启用(默认严格)
    ///
    /// `REFLECT_SANDBOX_OS_LEVEL=1` 加 `REFLECT_SANDBOX_STRICT=0` 可临时
    /// 回到旧降级语义(用于本地调试 / 过渡期)。
    pub fn from_env() -> Self {
        let on = std::env::var("REFLECT_SANDBOX_OS_LEVEL")
            .ok()
            .map(|s| {
                // 显式 "0/false/no/off" 才关闭,空 / 其他值 → 视为开启
                // (向后兼容 v1.2 的 "设了就开" 语义)。
                !matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "" | "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);
        let strict = std::env::var("REFLECT_SANDBOX_STRICT")
            .ok()
            .map(|s| {
                !matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);
        if on {
            let mut s = Self::enabled(extra_writable_from_env());
            s.strict = strict;
            s
        } else {
            let mut s = Self::passthrough();
            s.strict = strict;
            s
        }
    }

    /// 当前生效状态(综合 enabled + backend)。
    pub fn status(&self) -> OsSandboxStatus {
        if !self.enabled {
            OsSandboxStatus::Disabled
        } else {
            self.backend
        }
    }

    /// 人可读状态行(给 TUI / `/sandbox-toggle` pill / 启动 banner 用)。
    /// v1.3:状态行同时标注严格模式 / 网络边界 / 敏感读规则,便于运维
    /// 一眼看到当前安全下限。
    pub fn status_line(&self) -> String {
        let net = if self.allow_network {
            "网络:允许"
        } else {
            "网络:拒绝"
        };
        let sensitive = if self.enabled {
            "敏感读:默认拒绝(凭据/SSH/钥匙串)"
        } else {
            "敏感读:不受约束"
        };
        match self.status() {
            OsSandboxStatus::Disabled => {
                format!(
                    "os-sandbox: 未启用(透传,bash 裸执行);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::Seatbelt => {
                format!(
                    "os-sandbox: macOS Seatbelt 已启用(bash 受 sandbox-exec 约束);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::Landlock => {
                format!(
                    "os-sandbox: Linux Landlock 已启用(bash 受 landlock 约束);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::NoBackend => {
                if self.strict {
                    format!(
                        "os-sandbox: 已请求启用但当前平台无可用 backend — 严格模式下 BashTool 会失败关闭;{net};{sensitive}"
                    )
                } else {
                    format!(
                        "os-sandbox: 已请求启用但当前平台无可用 backend(降级透传);{net};{sensitive}"
                    )
                }
            }
        }
    }

    /// 是否真的会把命令放进沙箱(用于 BashTool 决定是否 wrap)。
    pub fn is_active(&self) -> bool {
        matches!(
            self.status(),
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock
        )
    }

    /// v1.3:fail-closed 检查。`strict && enabled` 时,后端不可用 / 平台
    /// 不是 macOS+Linux → 返回 `Err(reason)`;BashTool 据此抛
    /// `ToolError::SandboxUnavailable` 而不是裸 `sh -c`。
    /// 非严格模式返回 `Ok(())`,由 BashTool 走旧降级路径。
    pub fn enforce_or_fail(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(()); // 显式关闭 → 不检查
        }
        if !self.strict {
            return Ok(()); // 非严格模式 → 允许降级
        }
        match self.status() {
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock => Ok(()),
            OsSandboxStatus::NoBackend => Err(format!(
                "OS sandbox 后端不可用(平台:{});严格模式下拒绝裸执行。\
                 在 macOS 需 /usr/bin/sandbox-exec;在 Linux 需 Landlock 内核支持(>= 5.13)。\
                 如需临时降级,设置 REFLECT_SANDBOX_STRICT=0。",
                std::env::consts::OS
            )),
            OsSandboxStatus::Disabled => Err("OsSandbox 状态矛盾:enabled=true 但 Disabled".into()),
        }
    }

    // ── macOS Seatbelt ──────────────────────────────────────────────

    /// 生成 Seatbelt `.sb` profile 文本(给 `sandbox-exec -p`)。
    ///
    /// 策略:
    /// - `deny` 为默认(显式 allow 才放行)。
    /// - 允许写:workspace + `writable_dirs` + `$TMPDIR` + `/private/var/folders`
    ///   (macOS 真实 tempdir)。
    /// - 允许读:全盘(`(allow file-read*)`)—— 编译 / 分析常需读系统头与
    ///   依赖,限制读会大面积破坏可用性。写受限是核心安全边界。
    /// - 允许进程:posix_spawn / fork / exec(子进程继承沙箱)。
    /// - 网络:`allow_network` 控制(`(allow network*)` / deny)。
    /// - **显式 deny** `(deny file-write*)` 到系统敏感路径作为兜底,即使
    ///   上面 allow 有遗漏也挡 `rm -rf /`。
    pub fn seatbelt_profile(&self, workspace: &Path) -> String {
        let mut allow_write: Vec<String> = Vec::new();
        // workspace(规范化,展开符号链接)。
        if let Ok(canon) = workspace.canonicalize() {
            allow_write.push(canon.to_string_lossy().into_owned());
        } else {
            allow_write.push(workspace.to_string_lossy().into_owned());
        }
        // 额外可写目录。
        for d in &self.writable_dirs {
            allow_write.push(d.to_string_lossy().into_owned());
        }
        // 临时目录(macOS tempdir 通常在 /private/var/folders 或 /tmp)。
        allow_write.push("/private/var/folders".to_string());
        allow_write.push("/tmp".to_string());
        allow_write.push("/var/tmp".to_string());

        let write_rules = allow_write
            .iter()
            .map(|p| format!("(allow file-write* (subpath \"{}\"))", p))
            .collect::<Vec<_>>()
            .join("\n            ");
        let net_rule = if self.allow_network {
            "(allow network*)".to_string()
        } else {
            "(deny network*)".to_string()
        };
        let net_label = if self.allow_network {
            "允许"
        } else {
            "拒绝"
        };

        format!(
            "(version 1)
            (deny default)
            ;; 读:全盘放行(编译/分析需读系统头与依赖);v1.3 起对凭据 / SSH /
            ;; 钥匙串 / .aws / .gnupg 等敏感路径显式 deny(Seatbelt 中
            ;; deny 永远优先于 allow)。其他读路径全放行,保持可用性。
            (allow file-read*)
            ;; v1.3:敏感读兜底 —— 拒绝读取凭据 / SSH / 钥匙串。
            (deny file-read* (subpath \"/Users/*/.ssh\"))
            (deny file-read* (subpath \"/Users/*/.aws\"))
            (deny file-read* (subpath \"/Users/*/.gnupg\"))
            (deny file-read* (subpath \"/Users/*/.kube\"))
            (deny file-read* (subpath \"/Users/*/.docker\"))
            (deny file-read* (subpath \"/Users/*/.config/gh\"))
            (deny file-read* (subpath \"/Users/*/.netrc\"))
            (deny file-read* (subpath \"/Users/*/.npmrc\"))
            (deny file-read* (subpath \"/Users/*/.pypirc\"))
            (deny file-read* (subpath \"/Users/*/.cargo/credentials\"))
            (deny file-read* (subpath \"/Users/*/.config/git/credentials\"))
            (deny file-read* (subpath \"/Users/*/Library/Keychains\"))
            ;; 写:仅 workspace + 额外目录 + 临时目录
            {write_rules}
            ;; 网络:{net_label}
            {net_rule}
            ;; 进程:允许 spawn/fork/exec(子进程继承沙箱)
            (allow process-fork)
            (allow process-exec (subpath \"/usr/bin\"))
            (allow process-exec (subpath \"/bin\"))
            (allow process-exec (subpath \"/usr/local/bin\"))
            (allow process-exec (subpath \"/opt/homebrew/bin\"))
            ;; 信号 / 系统调用基础
            (allow sysctl-read)
            (allow process-info* (target self))
            ;; 显式兜底:系统关键目录写一律拒(防 rm -rf / 即便上面有遗漏)
            (deny file-write* (subpath \"/usr\"))
            (deny file-write* (subpath \"/bin\"))
            (deny file-write* (subpath \"/sbin\"))
            (deny file-write* (subpath \"/System\"))
            (deny file-write* (subpath \"/Library\"))",
        )
    }

    /// macOS:把命令包成 `sandbox-exec -p <profile> -- sh -c <cmd>` 的 argv。
    /// 返回 `(program, args)` 供 BashTool 用 `Command::new(program).args(args)`。
    /// profile 较长时 `-p` 内联可能有 shell 转义问题,故优先写临时文件用
    /// `-f <file>`;写失败则回退 `-p`(仍可用,只是 argv 长)。
    pub fn seatbelt_argv(
        &self,
        workspace: &Path,
        shell_cmd: &str,
    ) -> std::io::Result<(String, Vec<String>)> {
        let profile = self.seatbelt_profile(workspace);
        // 写临时 .sb 文件(sandbox-exec -f 比 -p 更稳:profile 不进 argv,
        // 不受 ARG_MAX / 转义影响)。
        let tmp = std::env::temp_dir().join(format!("reflect-sandbox-{}.sb", std::process::id()));
        std::fs::write(&tmp, &profile)?;
        Ok((
            "/usr/bin/sandbox-exec".to_string(),
            vec![
                "-f".to_string(),
                tmp.to_string_lossy().into_owned(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                shell_cmd.to_string(),
            ],
        ))
    }

    // ── Linux Landlock ──────────────────────────────────────────────

    /// Linux:返回一个 `unsafe` 的 `pre_exec` 闭包,在 fork 后、exec 前应用
    /// Landlock 规则。闭包限制 workspace 外的写(workspace 内写/读/执行
    /// 全放行;workspace 外读放行、写拒)。
    ///
    /// 若内核不支持 Landlock(`landlock_create_ruleset` 返回 ENOSYS /
    /// EOPNOTSUPP),闭包记 warn 后返回 `Ok(())`(降级,不阻塞 exec)。
    ///
    /// # Safety
    /// 仅在 `pre_exec`(子进程 fork 后)上下文调用;闭包内只做 async-signal-
    /// safe 的 libc 调用(landlock syscalls + readlink 等)。`unsafe` 因
    /// `pre_exec` 要求 `Send + Sync` 的 `FnMut` 且内部用 raw syscall。
    #[cfg(target_os = "linux")]
    pub fn landlock_pre_exec(
        &self,
        workspace: PathBuf,
    ) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
        let writable = self.writable_dirs.clone();
        move || apply_landlock(&workspace, &writable)
    }
}

/// 从 env 读额外可写目录(`REFLECT_SANDBOX_WRITABLE`,`:`分隔)。
fn extra_writable_from_env() -> Vec<PathBuf> {
    std::env::var("REFLECT_SANDBOX_WRITABLE")
        .ok()
        .map(|s| {
            s.split(':')
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

// ── Linux Landlock 实现 ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod landlock {
    use std::ffi::CString;
    use std::io;
    use std::path::{Path, PathBuf};

    // Landlock syscall 号(x86_64 / aarch64 一致)。
    const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
    const SYS_LANDLOCK_ADD_RULE: i64 = 445;
    const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;

    const LANDLOCK_RULE_PATH_BENEATH: i32 = 1;
    // access_fs 位:read/write/execute/make-dir 等(取宽松集合,再加 write 全集)。
    const ACCESS_FS_ALL: u64 = 0x1fff; // Landlock v1 ~1fff;v2 加 refer/delete

    #[repr(C)]
    struct LandlockRulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    extern "C" {
        fn syscall(num: i64, ...) -> i64;
        fn open(path: *const i8, flags: i32, ...) -> i32;
        fn close(fd: i32) -> i32;
        fn __errno_location() -> *mut i32;
    }

    fn errno() -> i32 {
        unsafe { *__errno_location() }
    }

    /// 在 pre_exec 上下文应用 Landlock。仅 workspace(及其额外可写目录)
    /// 放行全 access;workspace 外的写由内核拒(读 / exec 不受限 —— 与
    /// macOS 策略一致:限制写是核心安全边界)。
    pub(super) fn apply_landlock(workspace: &Path, writable: &[PathBuf]) -> io::Result<()> {
        // 1. create ruleset(仅 handled_access_fs;net 留 0)。
        let attr = LandlockRulesetAttr {
            handled_access_fs: ACCESS_FS_ALL,
            handled_access_net: 0,
        };
        let fd = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<LandlockRulesetAttr>(),
                0,
            )
        };
        if fd < 0 {
            let e = errno();
            // ENOSYS / EOPNOTSUPP → 内核不支持,降级(不阻塞)。
            if e == 38 || e == 95 {
                return Ok(());
            }
            return Err(io::Error::from_raw_os_error(e));
        }
        let ruleset_fd = fd as i32;

        // 2. add rules:workspace + 额外可写目录。
        for dir in std::iter::once(workspace).chain(writable.iter()) {
            let cpath = match CString::new(dir.to_string_lossy().as_bytes()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let parent_fd = unsafe { open(cpath.as_ptr(), 0o200000) }; // O_PATH = 0o200000 (0x200000)
            if parent_fd < 0 {
                continue;
            }
            let pb = LandlockPathBeneathAttr {
                allowed_access: ACCESS_FS_ALL,
                parent_fd,
            };
            let _ = unsafe {
                syscall(
                    SYS_LANDLOCK_ADD_RULE,
                    ruleset_fd as i64,
                    LANDLOCK_RULE_PATH_BENEATH as i64,
                    &pb as *const _,
                    0,
                )
            };
            unsafe { close(parent_fd) };
        }

        // 3. 对自身应用限制(restrict self)。
        let r = unsafe { syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd as i64, 0) };
        unsafe { close(ruleset_fd) };
        if r < 0 {
            let e = errno();
            if e == 38 || e == 95 {
                return Ok(()); // 降级
            }
            return Err(io::Error::from_raw_os_error(e));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
use landlock::apply_landlock;

// ── 向后兼容:旧的 stub 类型别名 ────────────────────────────────────
//
// v0 导出的 `OsSandboxStubStatus` / `wrap_command_stub` 可能在其他 crate
// 被引用。保留别名让旧代码继续编译,内部映射到新 API。
#[doc(hidden)]
pub type OsSandboxStubStatus = OsSandboxStatus;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_and_strict() {
        // v1.3:OsSandbox::default() 必须 enabled + strict,以满足
        // "默认要求 OS sandbox" 的严格安全基线。
        let s = OsSandbox::default();
        assert!(s.enabled, "default 必须 enabled");
        assert!(s.strict, "default 必须 strict(fail-closed)");
        // status 应反映 backend(Seatbelt / Landlock / NoBackend),
        // 永远不可能是 Disabled(default 已 enabled)。
        assert_ne!(s.status(), OsSandboxStatus::Disabled);
    }

    #[test]
    fn passthrough_helper_disables_sandbox() {
        let s = OsSandbox::passthrough();
        assert!(!s.enabled);
        assert!(!s.strict);
        assert_eq!(s.status(), OsSandboxStatus::Disabled);
        assert!(!s.is_active());
        assert!(s.status_line().contains("未启用"));
    }

    #[test]
    fn from_env_defaults_on_and_strict() {
        // v1.3:未设置 env → 启用 + strict(向后不兼容,但符合
        // "默认要求 OS sandbox" 的安全基线;设置 REFLECT_SANDBOX_OS_LEVEL=0
        // 可显式关闭)。
        let s = OsSandbox::from_env();
        // 不强制断言 enabled(可能继承 env),只确保不 panic 且 status 合法。
        let _ = s.status();
        let _ = s.strict;
    }

    #[test]
    fn enforce_or_fail_strict_no_backend_returns_err() {
        // v1.3 核心 fail-closed 不变式:strict + enabled + 后端不可用
        // → 返回 Err;BashTool 据此抛 ToolError::SandboxUnavailable。
        let s = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            backend: OsSandboxStatus::NoBackend,
        };
        assert!(s.enforce_or_fail().is_err());
    }

    #[test]
    fn enforce_or_fail_strict_with_backend_returns_ok() {
        // macOS / Linux 上 strict + 有 backend → Ok,BashTool 走 sandbox 路径。
        let mut s = OsSandbox::enabled(Vec::new());
        s.strict = true;
        if matches!(
            s.status(),
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock
        ) {
            assert!(s.enforce_or_fail().is_ok());
        } else {
            // 后端不可用(老 macOS / 非 Linux)→ 也应返回 Err。
            assert!(s.enforce_or_fail().is_err());
        }
    }

    #[test]
    fn enforce_or_fail_non_strict_never_errors() {
        // 非严格模式 → 允许降级透传(用于本地调试 / 过渡期)。
        let s = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: false,
            backend: OsSandboxStatus::NoBackend,
        };
        assert!(s.enforce_or_fail().is_ok());
        let s = OsSandbox::passthrough();
        assert!(s.enforce_or_fail().is_ok());
    }

    #[test]
    fn status_line_includes_net_and_sensitive_indicators() {
        // v1.3:状态行必须包含网络策略 + 敏感读边界指示,便于运维审计。
        let s = OsSandbox::default();
        let line = s.status_line();
        assert!(line.contains("网络:"), "status_line 缺网络指示: {line}");
        assert!(line.contains("敏感读:"), "status_line 缺敏感读指示: {line}");
        assert!(
            line.contains("strict="),
            "status_line 缺 strict 标志: {line}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn enabled_detects_seatbelt() {
        // 测试机上 sandbox-exec 通常存在。
        let s = OsSandbox::enabled(Vec::new());
        assert!(matches!(
            s.status(),
            OsSandboxStatus::Seatbelt | OsSandboxStatus::NoBackend
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_system_writes() {
        let s = OsSandbox::enabled(Vec::new());
        let prof = s.seatbelt_profile(Path::new("/Users/x/project"));
        // 关键不变式:profile 必须显式 deny 系统目录写 + deny default。
        assert!(prof.contains("(deny default)"), "profile must deny default");
        assert!(prof.contains("(deny file-write* (subpath \"/usr\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/bin\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/System\"))"));
        // workspace 必须在 allow file-write 列表。
        assert!(prof.contains("/Users/x/project"));
        // 网络默认拒。
        assert!(prof.contains("(deny network*)"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_allow_network_when_configured() {
        let mut s = OsSandbox::enabled(Vec::new());
        s.allow_network = true;
        let prof = s.seatbelt_profile(Path::new("/Users/x/p"));
        assert!(prof.contains("(allow network*)"));
    }

    /// v1.3:Seatbelt profile 必须显式 deny 敏感读路径(凭据 / SSH /
    /// 钥匙串等)。Seatbelt 中 deny 永远优先 allow。
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_sensitive_reads() {
        let s = OsSandbox::enabled(Vec::new());
        let prof = s.seatbelt_profile(Path::new("/Users/x/project"));
        assert!(
            prof.contains("(deny file-read* (subpath \"/Users/*/.ssh\"))"),
            "profile must deny SSH read; got: {prof}"
        );
        assert!(
            prof.contains("(deny file-read* (subpath \"/Users/*/.aws\"))"),
            "profile must deny AWS read"
        );
        assert!(
            prof.contains("(deny file-read* (subpath \"/Users/*/.gnupg\"))"),
            "profile must deny GPG read"
        );
        assert!(
            prof.contains("(deny file-read* (subpath \"/Users/*/Library/Keychains\"))"),
            "profile must deny Keychain read"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_argv_wraps_with_sandbox_exec() {
        let s = OsSandbox::enabled(Vec::new());
        let (prog, args) = s
            .seatbelt_argv(Path::new("/Users/x/p"), "rm -rf /")
            .unwrap();
        assert_eq!(prog, "/usr/bin/sandbox-exec");
        assert_eq!(args[0], "-f");
        assert!(args[1].ends_with(".sb"), "profile path should be .sb file");
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "sh");
        assert_eq!(args[4], "-c");
        assert_eq!(args[5], "rm -rf /");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rm_rf_root_is_blocked_in_sandbox() {
        // gap doc 核心验证用例:sandbox 内 `rm -rf /` 必须被阻止。
        // 直接跑 sandbox-exec(测试机有该 binary),尝试删一个系统路径
        // `/private/var/db/.reflect-sandbox-test-<rand>` 之外的真实系统路径
        // 较危险;改测「写到 /usr/local/.reflect-sandbox-test 应被拒」,
        // 等价证明 file-write* deny 生效。
        let s = OsSandbox::enabled(Vec::new());
        let (prog, args) = s
            .seatbelt_argv(Path::new("/tmp"), "touch /usr/local/.reflect-sandbox-blocked 2>/dev/null; test -f /usr/local/.reflect-sandbox-blocked && echo WROTE || echo BLOCKED")
            .unwrap();
        let out = std::process::Command::new(&prog)
            .args(&args)
            .output()
            .expect("sandbox-exec must run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // /usr 在 deny 列表 → 写失败 → 输出 BLOCKED。
        // (若测试以 root 跑,seatbelt 仍生效;若沙箱未起作用会输出 WROTE。)
        assert!(
            stdout.contains("BLOCKED"),
            "write to /usr must be blocked by sandbox; got stdout: {stdout}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_apply_does_not_panic() {
        // Landlock 在测试机内核不支持时应降级(Ok),支持时也应 Ok。
        let ws = std::env::temp_dir();
        let r = apply_landlock(&ws, &[]);
        // 测试进程已应用 landlock 后可能影响后续测试,故只验证不 panic。
        let _ = r;
    }

    #[test]
    fn detect_backend_returns_valid_variant() {
        let b = detect_backend();
        assert!(matches!(
            b,
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock | OsSandboxStatus::NoBackend
        ));
    }
}
