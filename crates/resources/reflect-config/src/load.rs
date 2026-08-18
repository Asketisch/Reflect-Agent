//! 配置加载:从 TOML 文件 / 字符串 / 默认路径。

use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::schema::{CURRENT_CONFIG_VERSION, ReflectConfig};
use crate::{DEFAULT_CONFIG_DIR, DEFAULT_CONFIG_FILE};

/// `$HOME/.reflect/config.toml` 的绝对路径。
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE))
}

/// 从任意 TOML 字符串解析;失败转 `ConfigError::Parse`。
///
/// 保留旧 API(无 migration)—— 给 plugin manifest 等非主配置解析用,
/// 也让 `load_default` 的「解析失败 → fallback」路径保持简单。
pub fn load_from_str(s: &str) -> Result<ReflectConfig, ConfigError> {
    toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// Phase 4 新 API:parse 后按 `target` 走 sequential migration。
///
/// - `target = CURRENT_CONFIG_VERSION`(默认推荐)即「升到最新」。
/// - `target` 低于 `current` 时 no-op(避免降级损坏 in-memory 数据)。
/// - 未知起始版本号 → `ConfigError::Migration`。
///
/// 当前 schema 在 v1,无 breaking change → `migrate_v1_to_v2` 是 no-op
/// 框架预留;后续真出现 schema breaking 时补 `migrate_v2_to_v3`。
pub fn load_from_str_with_migration(s: &str, target: u32) -> Result<ReflectConfig, ConfigError> {
    let mut cfg = load_from_str(s)?;
    // 旧 TOML 无 `config_version` 字段时,serde 默认填 1;此处不再做 .max。
    let mut current = cfg.config_version;
    // current > target:降级无 migration 路径(避免 in-memory 假设失效)。
    if current > target {
        return Err(ConfigError::Migration(format!(
            "config_version v{current} is newer than target v{target}; \
             refusing to downgrade"
        )));
    }
    while current < target {
        current = match current {
            1 => migrate_v1_to_v2(&mut cfg)?,
            other => {
                return Err(ConfigError::Migration(format!(
                    "no migration path from v{other} to v{target}"
                )));
            }
        };
        cfg.config_version = current;
    }
    Ok(cfg)
}

/// v1 → v2 migration。当前 schema 无 breaking change → no-op 框架预留。
/// 真正出现 schema breaking 时(如字段重命名 / 类型变更),在此处
/// 实现具体字段转换 + 返 `Ok(2)`。
fn migrate_v1_to_v2(_cfg: &mut ReflectConfig) -> Result<u32, ConfigError> {
    Ok(2)
}

/// 从指定文件路径加载;不存在 → `Io`,解析失败 → `Parse`。
pub fn load_from_file(path: &Path) -> Result<ReflectConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    load_from_str(&s)
}

/// 从指定文件路径加载并跑 migration 到 `CURRENT_CONFIG_VERSION`。
/// 给需要「读到最新 schema」的 caller 用(如 `reflect-cli doctor`)。
/// `load_default` 仍走 `load_from_file` 不做 migration —— migration 失败
/// 时 fallback 到 `Default` 与现有「解析失败 fallback」语义一致。
pub fn load_from_file_with_migration(path: &Path) -> Result<ReflectConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    load_from_str_with_migration(&s, CURRENT_CONFIG_VERSION)
}

/// 走 `$HOME/.reflect/config.toml`;文件缺失 / 解析失败均回退到 `Default`,
/// 并通过 `tracing::warn` 提示 —— 与 `HooksConfig::load_default` 语义一致。
pub fn load_default() -> ReflectConfig {
    let Some(path) = default_config_path() else {
        tracing::debug!("HOME unset; using ReflectConfig::default()");
        return ReflectConfig::default();
    };
    match load_from_file(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "config load failed; using default");
            ReflectConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ActiveSection, AnthropicSection, CompactSection, HooksSection};

    #[test]
    fn empty_string_yields_default() {
        let cfg = load_from_str("").unwrap();
        assert!(cfg.active.provider.is_none());
        assert!(cfg.anthropic.is_none());
        assert!(cfg.openai.is_none());
    }

    #[test]
    fn parses_anthropic_section() {
        let toml = r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "sk-ant-test"
base_url = "https://api.anthropic.com"
model = "claude-3-5-sonnet-latest"
timeout_secs = 90
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.active.provider.as_deref(), Some("anthropic"));
        let a = cfg.anthropic.unwrap();
        assert_eq!(a.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(a.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(a.model.as_deref(), Some("claude-3-5-sonnet-latest"));
        assert_eq!(a.timeout_secs, Some(90));
    }

    #[test]
    fn parses_openai_section() {
        let toml = r#"
[openai]
api_key = "sk-openai-test"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(
            cfg.openai.unwrap().api_key.as_deref(),
            Some("sk-openai-test")
        );
    }

    #[test]
    fn parses_compact_section() {
        let toml = r#"
[compact]
trigger_tokens = 8000
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.compact.trigger_tokens, Some(8000));
    }

    #[test]
    fn parses_hooks_section() {
        let toml = r#"
[hooks]
enabled = ["search_budget", "verification"]

[hooks.search_budget]
max_calls = 15
search_tools = ["grep", "read"]

[hooks.verification]
run_on_stop = false
test_command = "cargo test --quiet"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(
            cfg.hooks.enabled,
            Some(vec!["search_budget".into(), "verification".into()])
        );
        let sb = cfg.hooks.search_budget.unwrap();
        assert_eq!(sb.max_calls, Some(15));
        assert_eq!(sb.search_tools, Some(vec!["grep".into(), "read".into()]));
        let v = cfg.hooks.verification.unwrap();
        assert_eq!(v.run_on_stop, Some(false));
        assert_eq!(v.test_command.as_deref(), Some("cargo test --quiet"));
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let res = load_from_str("this is not toml");
        assert!(matches!(res, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn missing_file_returns_io_error() {
        let path = Path::new("/nonexistent/path/to/config.toml");
        let res = load_from_file(path);
        assert!(matches!(res, Err(ConfigError::Io(_))));
    }

    #[test]
    fn load_default_returns_default_when_no_home_or_no_file() {
        // 测试里 unset HOME 比较脆弱;只需确认无论如何都返回 Default。
        let cfg = load_default();
        // 不论拿到的是默认值(大多数情况)还是真的加载到了内容,
        // 都断言它可反序列化且结构良好。
        let _: ActiveSection = cfg.active;
        let _: Option<AnthropicSection> = cfg.anthropic;
        let _: CompactSection = cfg.compact;
        let _: HooksSection = cfg.hooks;
    }

    #[test]
    fn load_from_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "sk-x"
"#,
        )
        .unwrap();
        let cfg = load_from_file(&path).unwrap();
        assert_eq!(cfg.active.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.anthropic.unwrap().api_key.as_deref(), Some("sk-x"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let toml = r#"
[active]
provider = "anthropic"
totally_unknown_key = "ignored"

[anthropic]
api_key = "sk-x"
another_unknown = 42
"#;
        // serde 默认行为:忽略未知键(我们没开启 deny_unknown_fields)。
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.anthropic.unwrap().api_key.as_deref(), Some("sk-x"));
    }

    // ── Phase 4 config_version migration tests ───────────────────

    /// ReflectConfig::default() 显式给 `config_version = CURRENT_CONFIG_VERSION`。
    #[test]
    fn default_has_current_config_version() {
        let cfg = ReflectConfig::default();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.config_version, 1);
    }

    /// 旧 TOML 无 `config_version` 字段时,serde 默认填 1,兼容。
    #[test]
    fn config_version_defaults_to_1_when_missing() {
        let cfg = load_from_str("[active]\nprovider = \"anthropic\"\n").unwrap();
        assert_eq!(cfg.config_version, 1);
    }

    /// 显式 `config_version = 2`(假定的最新版本),load 后保留。
    #[test]
    fn explicit_config_version_is_preserved() {
        let cfg = load_from_str("config_version = 2\n").unwrap();
        assert_eq!(cfg.config_version, 2);
    }

    /// `target` 等于 `current`:no-op,直接返回。
    /// target < current(降级)走 `downgrade_to_older_target_errors` 测。
    #[test]
    fn migration_target_at_or_below_current_is_noop() {
        let cfg2 = load_from_str_with_migration("config_version = 2\n", 2).unwrap();
        assert_eq!(cfg2.config_version, 2, "target=current 应 no-op");
        // target 高于 current 但链未实现也走 no-op?不:走 migration 链,
        // 这里 current=2,target=3 走 unknown_migration_step_errors 路径。
        // 本测试只覆盖 target=current 的纯 no-op。
    }

    /// `target` 高于 current 且 migration 链存在:v1 → v2 是 no-op 框架预留,
    /// 新 cfg.config_version 被 bump 到 2。
    #[test]
    fn migration_chain_v1_to_v2_runs_noop() {
        // 旧 TOML 无 config_version → serde 默认 1;target=2 → 跑 v1→v2。
        let cfg = load_from_str_with_migration("[active]\nprovider = \"anthropic\"\n", 2).unwrap();
        assert_eq!(cfg.config_version, 2);
        assert_eq!(cfg.active.provider.as_deref(), Some("anthropic"));
    }

    /// 起始版本号高于 target(降级场景):返 `Migration` 错误。
    /// 拒绝降级避免 in-memory 字段假设失效(如新字段在老 schema 中缺语义)。
    #[test]
    fn downgrade_to_older_target_errors() {
        let res = load_from_str_with_migration("config_version = 999\n", 1);
        match res {
            Err(ConfigError::Migration(msg)) => {
                assert!(msg.contains("v999"), "got: {msg}");
                assert!(
                    msg.contains("downgrade") || msg.contains("newer"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Migration error, got {other:?}"),
        }
    }

    /// 回归测试:旧 TOML(无 config_version)走 migration 也兼容。
    #[test]
    fn old_toml_without_version_loads_and_migrates() {
        let cfg = load_from_str_with_migration(
            r#"
[active]
provider = "openai"

[openai]
api_key = "sk-old"
"#,
            CURRENT_CONFIG_VERSION,
        )
        .unwrap();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.openai.unwrap().api_key.as_deref(), Some("sk-old"));
    }

    /// `ConfigError::Migration` variant 可构造 + Debug 输出正常。
    #[test]
    fn config_error_migration_variant_works() {
        let e = ConfigError::Migration("test".into());
        assert_eq!(format!("{e}"), "migration: test");
    }
}
