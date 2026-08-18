//! `HookEngine` 的 TOML 配置加载器。
//!
//! 配置 schema 参见 `docs/tools-and-hooks.md §5.5`。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use reflect_config::HooksSection;

use crate::builtins::{
    LangfuseTracker, PlanCompletionHook, SearchBudgetHook, TestRunnerHook, VerificationHook,
};
use crate::engine::HookEngine;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
}

/// 顶层 hooks 配置(toml 中的 `[hooks]` 表)。
#[derive(Debug, Default, Deserialize)]
pub struct HooksConfig {
    /// 已启用 hook 的名字(其余会被跳过)。`None` 表示全部启用。
    pub enabled: Option<Vec<String>>,
    #[serde(default)]
    pub search_budget: Option<SearchBudgetConfig>,
    #[serde(default)]
    pub test_runner: Option<TestRunnerConfig>,
    #[serde(default)]
    pub plan_completion: Option<PlanCompletionConfig>,
    #[serde(default)]
    pub verification: Option<VerificationConfig>,
    #[serde(default)]
    pub langfuse_tracker: Option<LangfuseConfig>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct SearchBudgetConfig {
    pub max_calls: Option<u32>,
    pub search_tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct TestRunnerConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct PlanCompletionConfig {
    pub strict: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct VerificationConfig {
    pub run_on_stop: Option<bool>,
    pub test_command: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct LangfuseConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub public_key: Option<String>,
    pub secret_key: Option<String>,
    pub export_mode: Option<String>,
}

impl HooksConfig {
    /// 从 TOML 文件加载。
    pub fn load_from_file(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        toml::from_str(&s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// 从 `~/.reflect/config.toml` 加载,失败时回退到默认值。
    ///
    /// **自 M7 起已弃用**:推荐 [`ReflectConfig::load_default`](reflect_config::ReflectConfig::load_default)
    /// 加 [`HooksConfig::from_reflect_section`] 转换。等统一配置成为规范路径后,
    /// 本入口会在 M8+ 删除。
    #[deprecated(
        since = "0.0.0",
        note = "use reflect_config::ReflectConfig::load_default + HooksConfig::from_reflect_section"
    )]
    pub fn load_default() -> Self {
        let home = match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h),
            Err(_) => return Self::default(),
        };
        let path = home.join(".reflect").join("config.toml");
        #[allow(clippy::manual_unwrap_or_default)] // pre-M5: explicit match for clarity
        match Self::load_from_file(&path) {
            Ok(c) => c,
            Err(_) => Self::default(),
        }
    }

    /// 从统一配置 (`reflect_config::HooksSection`) 构造 `HooksConfig`。
    /// 各 `Option<>` 字段缺失时回退到 `Default` —— 与 `build_engine` 的 per-field 默认值语义对齐。
    pub fn from_reflect_section(s: &HooksSection) -> Self {
        Self {
            enabled: s.enabled.clone(),
            search_budget: s.search_budget.as_ref().map(|x| SearchBudgetConfig {
                max_calls: x.max_calls,
                search_tools: x.search_tools.clone(),
            }),
            test_runner: s
                .test_runner
                .as_ref()
                .map(|x| TestRunnerConfig { enabled: x.enabled }),
            plan_completion: s
                .plan_completion
                .as_ref()
                .map(|x| PlanCompletionConfig { strict: x.strict }),
            verification: s.verification.as_ref().map(|x| VerificationConfig {
                run_on_stop: x.run_on_stop,
                test_command: x.test_command.clone(),
            }),
            langfuse_tracker: s.langfuse_tracker.as_ref().map(|x| LangfuseConfig {
                enabled: x.enabled,
                endpoint: x.endpoint.clone(),
                public_key: x.public_key.clone(),
                secret_key: x.secret_key.clone(),
                export_mode: x.export_mode.clone(),
            }),
        }
    }

    /// 从本配置构建 `HookEngine`。未启用或未知的 hook 不会被注册。
    pub fn build_engine(&self) -> HookEngine {
        let engine = HookEngine::new();
        let enabled: Option<HashSet<String>> =
            self.enabled.as_ref().map(|v| v.iter().cloned().collect());

        let is_enabled = |name: &str| -> bool {
            match &enabled {
                Some(set) => set.contains(name),
                None => true,
            }
        };

        if is_enabled("search_budget") {
            let cfg = self.search_budget.clone().unwrap_or_default();
            let tools: HashSet<String> = cfg
                .search_tools
                .unwrap_or_else(|| vec!["grep".into(), "glob".into(), "read".into()])
                .into_iter()
                .collect();
            engine.register(SearchBudgetHook::new(cfg.max_calls.unwrap_or(20), tools));
        }
        if is_enabled("test_runner") {
            let cfg = self.test_runner.clone().unwrap_or_default();
            engine.register(TestRunnerHook::new(cfg.enabled.unwrap_or(true)));
        }
        if is_enabled("plan_completion") {
            let cfg = self.plan_completion.clone().unwrap_or_default();
            engine.register(PlanCompletionHook::new(cfg.strict.unwrap_or(true)));
        }
        if is_enabled("verification") {
            let cfg = self.verification.clone().unwrap_or_default();
            engine.register(VerificationHook::new(
                cfg.run_on_stop.unwrap_or(true),
                cfg.test_command.unwrap_or_else(|| "cargo test".into()),
            ));
        }
        if is_enabled("langfuse_tracker") {
            let cfg = self.langfuse_tracker.clone().unwrap_or_default();
            engine.register(LangfuseTracker::new(cfg));
        }
        engine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_engine_has_all_5_hooks() {
        let engine = HooksConfig::default().build_engine();
        assert_eq!(engine.len(), 5);
    }

    #[test]
    fn enabled_filter_respected() {
        let cfg = HooksConfig {
            enabled: Some(vec!["search_budget".into()]),
            ..Default::default()
        };
        let engine = cfg.build_engine();
        assert_eq!(engine.len(), 1);
    }

    #[test]
    fn custom_search_budget_values() {
        let cfg = HooksConfig {
            search_budget: Some(SearchBudgetConfig {
                max_calls: Some(5),
                search_tools: Some(vec!["grep".into()]),
            }),
            ..Default::default()
        };
        let engine = cfg.build_engine();
        assert_eq!(engine.len(), 5); // still 5 default-registered; config didn't restrict
    }

    #[test]
    fn parses_realistic_toml() {
        let toml = r#"
[search_budget]
max_calls = 10
search_tools = ["grep", "read"]

[plan_completion]
strict = false

[verification]
run_on_stop = true
test_command = "cargo test --no-fail-fast"
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.search_budget.unwrap().max_calls, Some(10));
        assert!(!cfg.plan_completion.unwrap().strict.unwrap());
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let toml = "this is not toml";
        let result: Result<HooksConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    /// M7:与 `reflect_config::HooksSection`(统一配置 schema)做往返转换。
    #[test]
    fn from_reflect_section_round_trip() {
        let toml = r#"
[hooks]
enabled = ["search_budget", "verification"]

[hooks.search_budget]
max_calls = 12
search_tools = ["grep", "read"]

[hooks.verification]
run_on_stop = true
test_command = "cargo test --quiet"
"#;
        let cfg: reflect_config::ReflectConfig = toml::from_str(toml).unwrap();
        let hooks = HooksConfig::from_reflect_section(&cfg.hooks);
        assert_eq!(
            hooks.enabled.as_deref(),
            Some(&["search_budget".to_string(), "verification".to_string()][..])
        );
        let sb = hooks.search_budget.unwrap();
        assert_eq!(sb.max_calls, Some(12));
        assert_eq!(sb.search_tools, Some(vec!["grep".into(), "read".into()]));
        let v = hooks.verification.unwrap();
        assert_eq!(v.run_on_stop, Some(true));
        assert_eq!(v.test_command.as_deref(), Some("cargo test --quiet"));
    }

    #[test]
    fn from_reflect_section_empty_section_yields_default() {
        use reflect_config::HooksSection;
        let empty = HooksSection::default();
        let hooks = HooksConfig::from_reflect_section(&empty);
        assert!(hooks.enabled.is_none());
        assert!(hooks.search_budget.is_none());
        assert!(hooks.test_runner.is_none());
    }

    #[test]
    fn from_reflect_section_then_build_engine_matches_default() {
        use reflect_config::HooksSection;
        let empty = HooksSection::default();
        let hooks = HooksConfig::from_reflect_section(&empty);
        let engine = hooks.build_engine();
        assert_eq!(engine.len(), 5);
    }
}
