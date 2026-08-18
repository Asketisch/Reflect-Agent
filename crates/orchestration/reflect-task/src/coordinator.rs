//! Coordinator 模式 —— 把 `reflect-task` crate 升级为多 agent 协调中枢。
//!
//! v1.1.0 Phase 4 落地。行为契约:
//! - **启用开关**:`REFLECT_COORDINATOR_MODE` env var 或
//!   `~/.reflect/config.toml` 的 `[coordinator] enabled = true`(config 优先)。
//! - **System prompt**:`DEFAULT_COORDINATOR_PROMPT` 嵌入二进制,
//!   可由 `[coordinator] system_prompt_path` 指向的文件覆盖。
//! - **Worker 工具白名单**:`build_worker_tool_registry` 从父 registry
//!   复制工具,排除 `INTERNAL_WORKER_TOOLS`(`TeamCreate` / `TeamDelete`
//!   / `send_message`)。
//! - **Scratchpad**:`build_scratchpad_path` 路径格式
//!   `/tmp/reflect-<uid>/<sanitized-cwd>/<session_id>/scratchpad`,
//!   session 启动期 `mkdir -p` 建好,SessionStart hook 触发。
//!
//! ## 模块组织
//! - 本文件:CoordinatorConfig + is_coordinator_enabled + build_worker_tool_registry
//!   + build_scratchpad_path + DEFAULT_COORDINATOR_PROMPT 常量。
//! - `coordinator/coordinator_prompt.md`:默认 prompt 资产,`include_str!` 嵌入。
//!
//! ## 关键设计取舍
//! - **默认 prompt 嵌入二进制**:与 `reflect_skills::catalog::BUILTIN_CATALOG`
//!   同语义,确保 `REFLECT_COORDINATOR_MODE=1` 离线工作;config 路径覆盖
//!   优先于默认。
//! - **excluded 列表 hard-coded**:不暴露 config 字段改 internal tools。
//!   用户应通过 system prompt 调整 coordinator 行为,而不是绕过白名单。

use std::path::{Path, PathBuf};

use tracing::warn;

// v1.1.0 Phase 4:把 worker 工具白名单搬到 `reflect-subagent::worker_registry`
// 避免反向依赖(本 crate 已依赖 reflect-subagent)。下方 re-export 保持
// 外部 API 不变 —— 现有 `reflect_task::coordinator::build_worker_tool_registry`
// 引用与 11 个单测无需改动。
pub use reflect_subagent::worker_registry::{INTERNAL_WORKER_TOOLS, build_worker_tool_registry};

/// 默认 coordinator system prompt —— `include_str!` 在编译期嵌入。
pub const DEFAULT_COORDINATOR_PROMPT: &str = include_str!("coordinator/coordinator_prompt.md");

/// Worker 子 agent 的默认 role 名(`call_<role>` 中的 role)。
pub const WORKER_ROLE: &str = "worker";

/// 默认最大并行 worker 数。`REFLECT_COORDINATOR_MAX_WORKERS` env var
/// 或 `[coordinator] max_workers` 可覆盖。
pub const DEFAULT_MAX_WORKERS: u8 = 4;

/// `max_workers` 的硬上限 —— 防御性 clamp,避免配置错值(如 255 或
/// 极端大数)让协调器一次性 spawn 过多 worker 撑爆资源。`from_env_or_config`
/// 末尾会把超界值降回 `DEFAULT_MAX_WORKERS`,保留合理范围 `[1, 32]`。
pub const MAX_WORKERS_CAP: u8 = 32;

/// 单 worker 一次最多持有的 task 数(v1.1.0 默认 1,留 v1.2 调整)。
///
/// scratchpad note 文件名上限。Worker 用 `<name>.md` 在 scratchpad 目录
/// 下记录发现/上下文,过长的文件名容易触发 fs 限制,统一压到 64 字符。
pub const MAX_NOTE_NAME_LEN: usize = 64;

/// scratchpad note 单次写入 body 上限(字节)。超出会被 `WriteNote` 工具
/// 截断并 `metadata.truncated = true`。16 KiB 与一次工具返回块大小
/// 持平,避免巨型 note 撑爆 LLM context。
pub const MAX_NOTE_BODY_BYTES: usize = 16 * 1024;

/// 校验 scratchpad note 名:只允许 kebab-case(`[a-z0-9_-]+`),
/// 长度 1..=`MAX_NOTE_NAME_LEN`。与 `validate_team_name` 字符规则一致,
/// 但允许 `-` 开头(便于命名空间)。
pub fn validate_note_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("note name cannot be empty".into());
    }
    if name.len() > MAX_NOTE_NAME_LEN {
        return Err(format!(
            "note name '{name}' exceeds {MAX_NOTE_NAME_LEN} chars"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "note name '{name}' must be kebab-case (lowercase a-z, 0-9, '-', '_')"
        ));
    }
    Ok(())
}

/// scratchpad 路径前缀的 `reflect-{id}` 部分 —— 路径格式
/// `/tmp/reflect-<uid>/<sanitized-cwd>/<session_id>/scratchpad`,但避免引入 `libc` / `nix` 依赖:`std` 已暴露
/// `process::id()` (PID),跨用户隔离由 `$TMPDIR` / `/tmp` 自身的
/// per-user 权限位(700 模式)保证,PID 已足够在同一用户下区分多 session。
///
/// 注:PID 在 session 生命周期内不变,但 scratchpad 路径与 `session_id`
/// (`ThreadId` UUID) 同段,不会与其它 session 串。
///
/// v1.1.0 review P2-2:旧名 `current_session_id` 与 `session_id` 参数
/// 极易混淆(一个返 PID,一个传 UUID),改名 `current_pid` 让 caller 一眼
/// 分清两段身份(`reflect-<pid>/<sanitized>/<uuid>/scratchpad`)。
fn current_pid() -> u32 {
    std::process::id()
}

/// 检查 coordinator 模式是否启用 —— 仅看 env var。`CoordinatorConfig::from_env_or_config`
/// 综合 env + config 后才是最终决策。
pub fn is_coordinator_enabled() -> bool {
    std::env::var("REFLECT_COORDINATOR_MODE")
        .map(|v| is_truthy_env_value(&v))
        .unwrap_or(false)
}

/// 严格 truthy 解析 —— 暴露 `pub(crate)` 以便 `is_coordinator_enabled` 与单测
/// 共用同一份规则。详细语义见下;`is_coordinator_enabled` 是该规则的 wrapper。
pub(crate) fn is_truthy_env_value(v: &str) -> bool {
    if v.is_empty() {
        return false;
    }
    let lower = v.to_lowercase();
    if lower == "0" || lower == "false" || lower == "no" || lower == "off" {
        return false;
    }
    true
}

/// Coordinator 模式的运行时配置 —— 由 `CoordinatorConfig::from_env_or_config`
/// 构造,`reflect_exec` 启动时单例化。
///
/// # 字段优先级
/// 1. `[coordinator].enabled`(`None` 时回退 env var)
/// 2. `[coordinator].system_prompt_path`(`None` 时用 `DEFAULT_COORDINATOR_PROMPT`)
/// 3. `[coordinator].max_workers`(`None` 时用 env var / `DEFAULT_MAX_WORKERS`)
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// 启用状态 —— 由 `from_env_or_config` 决定。
    pub enabled: bool,
    /// coordinator system prompt。默认 = `DEFAULT_COORDINATOR_PROMPT`;
    /// config 提供 `system_prompt_path` 时读取该文件覆盖。
    pub system_prompt: String,
    /// worker 子 agent 默认 role(`call_<role>` 后缀)。
    pub worker_subagent_type: String,
    /// 最大并行 worker 数。
    pub max_workers: u8,
    /// scratchpad 根路径 —— 在 `bootstrap_m4` / SessionStart hook 阶段
    /// 由 `build_scratchpad_path` 算并 `mkdir -p` 建好。
    pub scratchpad_root: Option<PathBuf>,
}

impl CoordinatorConfig {
    /// 综合 env var + `ReflectConfig` 构造 —— config 优先,env 兜底。
    ///
    /// 规则:
    /// - `enabled`:config 显式 `Some(_)` → 用 config;否则走
    ///   `is_coordinator_enabled()`(env var)。
    /// - `system_prompt_path`:config 显式 `Some(path)` → 读文件,失败 `warn`
    ///   并回退默认。
    /// - `max_workers`:config 显式 `Some(n)` → 用 config;否则走
    ///   `REFLECT_COORDINATOR_MAX_WORKERS` env var(数字);否则默认 4。
    pub fn from_env_or_config(cfg: &reflect_config::CoordinatorSection) -> Self {
        let enabled = cfg.enabled.unwrap_or_else(is_coordinator_enabled);

        // system prompt:config 路径覆盖默认。
        let system_prompt = if let Some(path) = cfg.system_prompt_path.as_ref() {
            match std::fs::read_to_string(path) {
                Ok(s) if !s.trim().is_empty() => {
                    tracing::info!(
                        path = %path.display(),
                        "coordinator: 使用 config 提供的 system prompt"
                    );
                    s
                }
                Ok(_) => {
                    warn!(path = %path.display(),
                        "coordinator: config system_prompt_path 指向空文件,使用默认");
                    DEFAULT_COORDINATOR_PROMPT.to_string()
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e,
                        "coordinator: 读 system_prompt_path 失败,使用默认");
                    DEFAULT_COORDINATOR_PROMPT.to_string()
                }
            }
        } else {
            DEFAULT_COORDINATOR_PROMPT.to_string()
        };

        // max_workers:config > env > 默认。末尾 clamp 到 [1, MAX_WORKERS_CAP]
        // 防御极端配置值。
        let max_workers = cfg
            .max_workers
            .unwrap_or_else(|| {
                std::env::var("REFLECT_COORDINATOR_MAX_WORKERS")
                    .ok()
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(DEFAULT_MAX_WORKERS)
            })
            .clamp(1, MAX_WORKERS_CAP);

        Self {
            enabled,
            system_prompt,
            worker_subagent_type: WORKER_ROLE.to_string(),
            max_workers,
            scratchpad_root: None,
        }
    }
}

// `build_worker_tool_registry` 与 `INTERNAL_WORKER_TOOLS` 已 re-export 自
// `reflect_subagent::worker_registry`(见文件顶部)。原因:v1.1.0 Phase 4 把
// spawn 时调用的白名单逻辑搬到 spawn 所在 crate,避免 `reflect-subagent`
// 反向依赖 `reflect-task`(后者已依赖前者)。

/// 构造 scratchpad 路径 —— 格式
/// `/tmp/reflect-<uid>/<sanitized-cwd>/<session_id>/scratchpad`。
///
/// `cwd` 用作 namespace(同一 workspace 不同 session 共享一组 scratchpads);
/// `session_id` 是 `ThreadId` 的字符串形式(UUID v4)。
///
/// `sanitize_cwd_for_path` 把绝对路径里的 `/` 替换为 `-`,并去掉前导 `-`,
/// 保证路径扁平无嵌套。空 `session_id` 兜底 `"unknown"`。
pub fn build_scratchpad_path(cwd: &Path, session_id: &str) -> PathBuf {
    let pid = current_pid();
    let sanitized = sanitize_cwd_for_path(cwd);
    let sid = if session_id.is_empty() {
        "unknown"
    } else {
        session_id
    };
    PathBuf::from(format!("/tmp/reflect-{pid}/{sanitized}/{sid}/scratchpad"))
}

/// 把绝对 cwd 转成单段路径:去掉前导 `/` 与尾随 `/`,中间 `/` 或 `\`
/// 替换为 `-`,便于 `/tmp/reflect-<pid>/<sanitized>/<sid>/scratchpad`
/// 形式拼接。
///
/// 例:`/Users/me/proj` → `Users-me-proj`
///     `/Users/me/proj/` → `Users-me-proj`
///     `C:\Users\me\proj` → `C:-Users-me-proj`
fn sanitize_cwd_for_path(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let trimmed = s.trim_matches(|c| c == '/' || c == '\\');
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_sep = false;
    for c in trimmed.chars() {
        if c == '/' || c == '\\' {
            if !prev_sep {
                out.push('-');
                prev_sep = true;
            }
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    out
}

/// 在 `bootstrap_m4` 阶段调:CoordinatorConfig 启用时建 scratchpad
/// 目录(sync fs 操作),失败仅 warn,不阻塞启动。
pub fn ensure_scratchpad(cfg: &CoordinatorConfig, cwd: &Path, session_id: &str) {
    if !cfg.enabled {
        return;
    }
    let path = build_scratchpad_path(cwd, session_id);
    match std::fs::create_dir_all(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "coordinator scratchpad created"),
        Err(e) => {
            warn!(path = %path.display(), error = %e,
                "coordinator: scratchpad mkdir 失败,worker 启动时再尝试");
        }
    }
}

// ── prompt builder 钩子 ────────────────────────────────────────────────────────

/// `PromptBuilder` 的 sections 拼接 —— 由 `reflect-prompt` 暴露 `add_section`
/// 后,`reflect-exec` 在 `is_coordinator_enabled` 时调 `add_section("coordinator",
/// &cfg.system_prompt)`,追加到 `core` layer 末尾。
///
/// 这里留一个轻量 helper 暴露给 `reflect-exec` 用 —— 不动 `reflect-prompt`
/// 的内部数据结构(避免暴露锁类型)。Phase 4 不重写 `PromptBuilder`;真正的
/// 拼接发生在 `reflect-exec::bootstrap_m4` 通过
/// `prompt_builder.add_section` 实现。
pub fn build_core_with_coordinator(
    base_core: &str,
    memory: &str,
    coordinator: Option<&str>,
) -> String {
    use reflect_prompt::LayeredPrompt;
    // 复用 `LayeredPrompt::compose_core` 的 memory 注入语义。
    let mut out = LayeredPrompt::compose_core(base_core, memory);
    if let Some(p) = coordinator
        && !p.trim().is_empty()
    {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str("## Coordinator Mode\n");
        out.push_str(p.trim());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── is_coordinator_enabled(检查是否启用 coordinator)──────────────

    /// env unset → false(走纯函数模拟)。
    #[test]
    fn is_enabled_unset_is_false() {
        assert!(!_is_enabled_match(None));
    }

    /// `REFLECT_COORDINATOR_MODE` 任意 truthy 值 → true(env var 测试)。
    ///
    /// 注:由于 `cargo test` 多线程跑,直接改 `REFLECT_COORDINATOR_MODE`
    /// 会污染同进程其它测试。这里改为测与生产逻辑完全等价的纯函数
    /// —— 任何 `is_truthy_env_value` 接受的值,生产 `is_coordinator_enabled`
    /// 都会接受。
    #[test]
    fn is_enabled_truthy_values() {
        for v in ["1", "true", "TRUE", "yes", "on", "anything"] {
            assert!(is_truthy_env_value(v), "value '{v}' should be truthy");
        }
    }

    /// 空串 / "0" / "false" / "no" / "off" → false(env var 测试)。
    ///
    /// v1.1.0 review P2-1:旧版 "no" / "off" 被算作 truthy(只屏蔽 "0" / "false"),
    /// 用户 `REFLECT_COORDINATOR_MODE=no` 期望关闭却开启,违反直觉。
    /// 修复后 `is_truthy_env_value` 把显式 falsy 集合扩到 {0, false, no, off}。
    #[test]
    fn is_enabled_falsy_set_values_are_false() {
        for v in ["", "0", "false", "FALSE", "no", "NO", "off", "OFF"] {
            assert!(!is_truthy_env_value(v), "value '{v}' should NOT be truthy");
        }
    }

    /// helper:不污染全局 env,与生产 `is_coordinator_enabled` 内部
    /// 逻辑 1:1 等价。返回 `Some(bool)` 模拟 env var presence。
    fn _is_enabled_match(env_value: Option<&str>) -> bool {
        env_value.map(is_truthy_env_value).unwrap_or(false)
    }

    // helper:纯函数,严格 truthy 解析。
    //
    // 接受:`"1"` / `"true"` / `"yes"` / `"on"`(大小写不敏感)以及
    // 任意非空非显式 falsy 的字符串(向后兼容:`REFLECT_COORDINATOR_MODE=anything`
    // 仍视为开启,保持原行为)。
    // 拒绝:`""` / `"0"` / `"false"` / `"no"` / `"off"`(大小写不敏感)。
    // 与生产 `is_coordinator_enabled` 内层表达式完全一致。

    // ── sanitize_cwd_for_path(路径清洗)─────────────────────────────

    /// 标准 unix 路径:`/Users/me/proj` → `Users-me-proj`。
    #[test]
    fn sanitize_cwd_strips_slashes() {
        assert_eq!(
            sanitize_cwd_for_path(Path::new("/Users/me/proj")),
            "Users-me-proj"
        );
    }

    /// 尾随 `/` 被去除。
    #[test]
    fn sanitize_cwd_strips_trailing_slash() {
        assert_eq!(
            sanitize_cwd_for_path(Path::new("/Users/me/proj/")),
            "Users-me-proj"
        );
    }

    /// 多级嵌套:`/a/b/c/d` → `a-b-c-d`。
    #[test]
    fn sanitize_cwd_deep_path() {
        assert_eq!(sanitize_cwd_for_path(Path::new("/a/b/c/d")), "a-b-c-d");
    }

    /// 反斜杠在 windows 形态下也转 `-`(unix 测试只跑平台无关断言)。
    #[test]
    fn sanitize_cwd_handles_backslash() {
        // 不依赖平台:仅断言 sanitize 函数本身把 `\\` 也处理掉。
        let p = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\proj")
        } else {
            PathBuf::from(r"/tmp/a\b\c")
        };
        let s = sanitize_cwd_for_path(&p);
        // 不含 `/` 或 `\`。
        assert!(!s.contains('/'), "got {s:?}");
        assert!(!s.contains('\\'), "got {s:?}");
    }

    // ── build_scratchpad_path(构造 scratchpad 路径)─────────────────

    /// scratchpad 路径格式:`/tmp/reflect-<pid>/<sanitized>/<sid>/scratchpad`。
    #[test]
    fn scratchpad_path_format() {
        let p = build_scratchpad_path(Path::new("/Users/me/proj"), "abcd-1234");
        let s = p.to_string_lossy().to_string();
        // 前缀严格 /tmp/reflect-{pid}/,pid 是数字。
        assert!(s.starts_with("/tmp/reflect-"), "got: {s}");
        assert!(s.contains("/Users-me-proj/"), "got: {s}");
        assert!(s.ends_with("/abcd-1234/scratchpad"), "got: {s}");
    }

    /// 空 session_id 兜底为 `unknown`。
    #[test]
    fn scratchpad_path_empty_session_id() {
        let p = build_scratchpad_path(Path::new("/x"), "");
        assert!(p.to_string_lossy().ends_with("/unknown/scratchpad"));
    }

    // ── INTERNAL_WORKER_TOOLS / build_worker_tool_registry 测试 ────────
    //
    // v1.1.0 Phase 4 把这两个常量 / 函数搬到 `reflect-subagent::worker_registry`,
    // 配套测试也搬过去。`reflect-task::coordinator` 顶部 re-export 保持外部
    // API 不变 —— 调用方路径无需修改。这里不再重复测试。

    // ── validate_note_name(校验 note 名)─────────────────────────────

    /// kebab-case 名字接受。
    #[test]
    fn validate_note_name_accepts_kebab() {
        assert!(validate_note_name("findings").is_ok());
        assert!(validate_note_name("with-dash").is_ok());
        assert!(validate_note_name("under_score").is_ok());
        assert!(validate_note_name("x1").is_ok());
    }

    /// 非法名字(大写 / 空格 / 点 / 超长)被拒。
    #[test]
    fn validate_note_name_rejects_invalid() {
        assert!(validate_note_name("").is_err());
        assert!(validate_note_name("BadName").is_err());
        assert!(validate_note_name("with space").is_err());
        assert!(validate_note_name("with.dot").is_err());
        assert!(validate_note_name(&"x".repeat(MAX_NOTE_NAME_LEN + 1)).is_err());
    }

    // ── max_workers clamp(上限钳制)───────────────────────────────

    /// `from_env_or_config` 把超界 `max_workers` 钳到 `[1, MAX_WORKERS_CAP]`。
    #[test]
    fn max_workers_clamped_to_safe_range() {
        use reflect_config::CoordinatorSection;
        let cfg = CoordinatorSection {
            enabled: Some(false),
            system_prompt_path: None,
            max_workers: Some(100), // 远超 cap
        };
        let out = CoordinatorConfig::from_env_or_config(&cfg);
        assert_eq!(out.max_workers, MAX_WORKERS_CAP, "100 应被钳到 32");

        let cfg_zero = CoordinatorSection {
            enabled: Some(false),
            system_prompt_path: None,
            max_workers: Some(0), // 非法 → 钳到 1
        };
        let out = CoordinatorConfig::from_env_or_config(&cfg_zero);
        assert_eq!(out.max_workers, 1, "0 应被钳到 1");

        let cfg_normal = CoordinatorSection {
            enabled: Some(false),
            system_prompt_path: None,
            max_workers: Some(8),
        };
        let out = CoordinatorConfig::from_env_or_config(&cfg_normal);
        assert_eq!(out.max_workers, 8, "正常值不变");
    }

    // ── DEFAULT_COORDINATOR_PROMPT(默认 prompt)─────────────────────

    /// 嵌入的 prompt 含 6 个段标识。
    #[test]
    fn default_prompt_contains_all_sections() {
        for section in [
            "Your Role",
            "Your Tools",
            "Workers",
            "Task Workflow",
            "Writing Worker Prompts",
            "Scratchpad",
            "Example Session",
        ] {
            assert!(
                DEFAULT_COORDINATOR_PROMPT.contains(section),
                "missing section: {section}"
            );
        }
    }

    // ── build_core_with_coordinator(拼接 core)───────────────────────

    /// 无 coordinator 时 = `LayeredPrompt::compose_core` 的等价结果。
    #[test]
    fn core_without_coordinator_matches_base() {
        let core = build_core_with_coordinator("you are an assistant", "## Facts\n- x", None);
        assert!(core.contains("you are an assistant"));
        assert!(core.contains("## Agent Memory"));
        assert!(!core.contains("## Coordinator Mode"));
    }

    /// 加 coordinator 时 = base + `## Coordinator Mode` + body。
    #[test]
    fn core_with_coordinator_appends_section() {
        let core = build_core_with_coordinator("base", "", Some("# Coordinator\nCustom rules."));
        assert!(core.contains("base"));
        assert!(core.contains("## Coordinator Mode"));
        assert!(core.contains("# Coordinator"));
    }

    /// 空 coordinator body 时不追加 section(避免空块)。
    #[test]
    fn core_with_empty_coordinator_body_skips_section() {
        let core = build_core_with_coordinator("base", "", Some("   \n  \n"));
        assert!(!core.contains("## Coordinator Mode"));
    }
}
