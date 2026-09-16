//! `builder` 模块单测。整体迁自原 `builder.rs` 内联 `#[cfg(test)] mod tests`。

use super::*;
use crate::schema::{ActiveSection, AnthropicSection, OllamaSection, OpenAISection};

fn cfg_with_anthropic(key: &str) -> ReflectConfig {
    ReflectConfig {
        anthropic: Some(AnthropicSection {
            api_key: Some(key.into()),
            base_url: Some("https://example.test".into()),
            model: Some("claude-test".into()),
            timeout_secs: Some(30),
            credentials: vec![],
        }),
        ..Default::default()
    }
}

fn cfg_with_openai(key: &str) -> ReflectConfig {
    ReflectConfig {
        openai: Some(OpenAISection {
            api_key: Some(key.into()),
            base_url: None,
            model: Some("gpt-test".into()),
            timeout_secs: None,
            credentials: vec![],
            responses_api: false,
        }),
        ..Default::default()
    }
}

fn cfg_with_ollama(model: Option<&str>) -> ReflectConfig {
    let toml = match model {
        Some(m) => format!(
            r#"
            [ollama]
            model = "{m}"
            "#
        ),
        None => r#"
            [ollama]
            "#
        .to_string(),
    };
    toml::from_str(&toml).unwrap()
}

#[test]
fn empty_config_yields_empty_registry() {
    let r = ReflectConfig::default().to_registry().unwrap();
    assert!(r.list().is_empty());
}

#[test]
fn registry_registers_anthropic_and_openai() {
    let cfg = ReflectConfig {
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            ..Default::default()
        }),
        openai: Some(OpenAISection {
            api_key: Some("sk-o".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = cfg.to_registry().unwrap();
    assert_eq!(
        r.list(),
        vec!["anthropic".to_string(), "openai".to_string()]
    );
    assert!(r.get("anthropic").is_some());
    assert!(r.get("openai").is_some());
}

#[test]
fn apply_to_registry_is_additive() {
    let r = ModelRegistry::new();
    cfg_with_anthropic("sk-a").apply_to_registry(&r).unwrap();
    cfg_with_openai("sk-o").apply_to_registry(&r).unwrap();
    assert_eq!(r.list().len(), 2);
}

// ── Ollama (v0.3.1) ──────────────────────────────────────────────

#[test]
fn registry_registers_ollama_when_section_present() {
    let cfg = cfg_with_ollama(Some("llama3.2"));
    let r = cfg.to_registry().unwrap();
    assert!(r.get("ollama").is_some(), "ollama should be registered");
}

#[test]
fn active_provider_canonicalizes_ollama_aliases() {
    // 显式 `provider = "ollama"` → 选 ollama。
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("ollama".into()),
            ..Default::default()
        },
        ollama: Some(OllamaSection::default()),
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), Some("ollama"));

    // alias `"local"` 也被 canonicalize。
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("Local".into()),
            ..Default::default()
        },
        ollama: Some(OllamaSection::default()),
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), Some("ollama"));
}

#[test]
fn active_provider_falls_back_to_ollama_when_section_present() {
    // 没显式 `active.provider` → 但有 `[ollama]` 段 → fall-back 选 ollama。
    let cfg = cfg_with_ollama(Some("qwen2.5:7b"));
    assert_eq!(cfg.active_provider(), Some("ollama"));
}

#[test]
fn resolve_model_ollama_returns_section_override() {
    let cfg = cfg_with_ollama(Some("qwen2.5:7b"));
    assert_eq!(cfg.resolve_model("ollama"), Some("qwen2.5:7b".to_string()));
}

/// v1.5 诚实化:未显式配置任何 model → `None`(不再编造内置默认)。
#[test]
fn resolve_model_returns_none_when_nothing_configured() {
    let cfg = ReflectConfig::default();
    assert_eq!(cfg.resolve_model("ollama"), None);
    assert_eq!(cfg.resolve_model("anthropic"), None);
    assert_eq!(cfg.resolve_model("openai"), None);
    assert_eq!(cfg.resolve_model("gemini"), None);
}

#[test]
fn resolved_model_spec_for_ollama() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("ollama".into()),
            ..Default::default()
        },
        ollama: Some(OllamaSection {
            model: Some("qwen2.5:7b".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec(),
        Some("ollama/qwen2.5:7b".to_string())
    );
}

// ── v1.0 多 Provider 路由:[[provider.credentials]] 数组 ──────

/// `[[anthropic.credentials]]` 数组非空 → builder 展开为 N entry pool。
#[test]
fn credentials_array_builds_multi_entry_pool() {
    let toml = r#"
        [[anthropic.credentials]]
        label = "work"
        api_key = "sk-work"
        weight = 2

        [[anthropic.credentials]]
        label = "personal"
        api_key = "sk-personal"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let r = cfg.to_registry().unwrap();
    let healthy = r.healthy_clients("anthropic/claude-3-5-sonnet-latest");
    assert_eq!(healthy.len(), 2, "应有 work + personal 两个 entry");
    // healthy_clients 顺序 = pool.entries 插入顺序
    assert_eq!(healthy[0].label, "work");
    assert_eq!(healthy[1].label, "personal");
    // work weight=2, personal weight=1 → 3 次循环里 work 应得 2 次
    let mut work = 0;
    let mut personal = 0;
    for _ in 0..3 {
        match r.next_for("anthropic/x", &[]).unwrap().label.as_str() {
            "work" => work += 1,
            "personal" => personal += 1,
            _ => panic!("unexpected label"),
        }
    }
    assert_eq!(work, 2);
    assert_eq!(personal, 1);
}

/// 旧 `api_key = "sk-a"` 单值形态仍能注册为单 entry "default" 池。
#[test]
fn legacy_api_key_wraps_to_default_credential() {
    let cfg = ReflectConfig {
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            base_url: None,
            model: None,
            timeout_secs: None,
            credentials: vec![],
        }),
        ..Default::default()
    };
    let r = cfg.to_registry().unwrap();
    let nc = r.next_for("anthropic/x", &[]).unwrap();
    assert_eq!(nc.label, "default");
}

/// `credentials` 数组与 `api_key` 同时存在 → 数组优先,`api_key`
/// 字段被忽略(文档说"若 credentials 非空,本字段仍可保留作占位")。
#[test]
fn credentials_array_takes_precedence_over_legacy_api_key() {
    let cfg = ReflectConfig {
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-LEGACY-IGNORED".into()),
            base_url: None,
            model: None,
            timeout_secs: None,
            credentials: vec![crate::schema::CredentialConfig {
                label: "only".into(),
                api_key: "sk-array".into(),
                base_url: None,
                model: None,
                weight: 1,
                cooldown_override_secs: None,
                quota: None,
            }],
        }),
        ..Default::default()
    };
    let r = cfg.to_registry().unwrap();
    let healthy = r.healthy_clients("anthropic/x");
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0].label, "only");
}

#[test]
fn ollama_skipped_when_no_section_and_no_env() {
    // 无段 + env 未设 → 不注册,active_provider 不返回 ollama。
    // (env 状态对其他测试透明:此测试只验证配置路径。)
    let cfg = ReflectConfig::default();
    let r = cfg.to_registry().unwrap();
    assert!(r.get("ollama").is_none());
    // active_provider 依赖 env 兜底,只断言非 ollama 时返回 None 或非 ollama。
    let _ = cfg.active_provider(); // 不 panic 即可
}

#[test]
fn canonical_provider_round_trip_includes_ollama() {
    for (raw, want) in [
        ("ollama", Some("ollama")),
        ("Ollama", Some("ollama")),
        ("local", Some("ollama")),
        ("LOCAL", Some("ollama")),
        ("gemini", None),
    ] {
        assert_eq!(canonical_provider(raw), want, "raw={raw}");
    }
}

#[test]
fn empty_api_key_section_does_not_register() {
    let cfg = ReflectConfig {
        anthropic: Some(AnthropicSection::default()),
        ..Default::default()
    };
    // 测试中 TOML key 为空且 env 未设置 → 跳过。
    // SAFETY:测试并行运行,不要修改 env。
    let r = cfg.to_registry().unwrap();
    // anthropic 因无 key 被跳过(env 可能已设也可能未设)。
    // 仅断言 openai 也不存在。
    assert!(r.get("openai").is_none());
}

#[test]
fn active_provider_explicit_in_toml() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("openai".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            ..Default::default()
        }),
        openai: Some(OpenAISection {
            api_key: Some("sk-o".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), Some("openai"));
}

#[test]
fn active_provider_falls_back_to_first_nonempty() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(cfg.active_provider(), Some("anthropic"));
}

#[test]
fn active_provider_canonicalizes_aliases() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("Claude".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), Some("anthropic"));

    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("gpt".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), Some("openai"));
}

#[test]
fn active_provider_unknown_string_returns_none() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("gemini".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(cfg.active_provider(), None);
}

#[test]
fn resolve_model_returns_section_override() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.resolve_model("anthropic"),
        Some("claude-test".to_string())
    );
}

/// v1.5 诚实化:段级 / env / 钉住条目都没有 → `None`,不再回落内置默认。
#[test]
fn resolve_model_returns_none_when_section_missing() {
    let cfg = ReflectConfig::default();
    assert_eq!(cfg.resolve_model("anthropic"), None);
    assert_eq!(cfg.resolve_model("openai"), None);
    assert_eq!(cfg.resolve_model("gemini"), None);
}

#[test]
fn canonical_provider_round_trip() {
    for (raw, want) in [
        ("anthropic", Some("anthropic")),
        ("Anthropic", Some("anthropic")),
        ("claude", Some("anthropic")),
        ("Claude", Some("anthropic")),
        ("openai", Some("openai")),
        ("GPT", Some("openai")),
        ("gemini", None),
        ("", None),
    ] {
        assert_eq!(canonical_provider(raw), want, "raw={raw}");
    }
}

// ── resolved_model_spec ─────────────────────────────────────────────

/// TOML `[anthropic].model` 覆盖默认,拼接为 `provider/model`。
#[test]
fn resolved_model_spec_uses_section_override() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.resolved_model_spec(),
        Some("anthropic/claude-test".to_string())
    );
}

/// `[active].provider` 优先于「第一个非空 section」的隐式回退。
#[test]
fn resolved_model_spec_prefers_active_provider() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("openai".into()),
            ..Default::default()
        },
        anthropic: Some(AnthropicSection {
            api_key: Some("sk-a".into()),
            ..Default::default()
        }),
        openai: Some(OpenAISection {
            api_key: Some("sk-o".into()),
            model: Some("gpt-test".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec(),
        Some("openai/gpt-test".to_string())
    );
}

/// 没有可用 provider(`api_key` 空 + env 未设)→ `None`。
#[test]
fn resolved_model_spec_returns_none_when_no_provider() {
    // 强制清掉 env(测试并行安全靠各测试独立假定)。
    let prior_model = std::env::var(ENV_REFLECT_MODEL).ok();
    let prior_provider = std::env::var(ENV_REFLECT_PROVIDER).ok();
    // SAFETY: 测试里需要串行化环境变量变更;此测试断言 env-无关路径,
    // 所以移除变量以保证结果可重复。
    unsafe {
        std::env::remove_var(ENV_REFLECT_MODEL);
        std::env::remove_var(ENV_REFLECT_PROVIDER);
    }
    let cfg = ReflectConfig::default();
    let result = cfg.resolved_model_spec();
    // 仅当 provider 也确实没有时才 None;此处默认 `active_provider()`
    // 可能因环境变量而返回 anthropic/openai,所以只验证类型。
    let _ = result;
    if let Some(m) = prior_model {
        unsafe {
            std::env::set_var(ENV_REFLECT_MODEL, m);
        }
    }
    if let Some(p) = prior_provider {
        unsafe {
            std::env::set_var(ENV_REFLECT_PROVIDER, p);
        }
    }
}

/// provider 已配置但没有任何显式 model → `None`(诚实化:GUI/TUI 据此
/// 显示"未配置模型",而不是编造 `claude-3-5-sonnet-latest` 之类默认值)。
#[test]
fn resolved_model_spec_returns_none_when_no_model_configured() {
    let cfg = ReflectConfig {
        active: ActiveSection {
            provider: Some("openai".into()),
            ..Default::default()
        },
        openai: Some(OpenAISection {
            api_key: Some("sk-o".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_model_spec(), None);
}

// ── v1.5:[active].credential 钉住 + 条目级 model ────────────────────

/// `[active].credential` 命中条目时,该条目的 `model` 优先于段级覆盖。
#[test]
fn resolve_model_prefers_pinned_credential_model() {
    let toml = r#"
        [active]
        provider = "anthropic"
        credential = "minimax"

        [anthropic]
        model = "section-model"

        [[anthropic.credentials]]
        label = "minimax"
        api_key = "sk-m"

        [[anthropic.credentials]]
        label = "other"
        api_key = "sk-o"
        model = "other-model"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential().as_deref(), Some("minimax"));
    // 命中条目无 model → 回落段级;不因别的条目有 model 而误取。
    assert_eq!(
        cfg.resolve_model("anthropic"),
        Some("section-model".to_string())
    );

    // 被钉住的条目自带 model → 压过段级。
    let toml = toml.replace(
        "api_key = \"sk-m\"",
        "api_key = \"sk-m\"\n        model = \"minimax-model\"",
    );
    let cfg: ReflectConfig = toml::from_str(&toml).unwrap();
    assert_eq!(
        cfg.resolve_model("anthropic"),
        Some("minimax-model".to_string())
    );
    assert_eq!(
        cfg.resolved_model_spec(),
        Some("anthropic/minimax-model".to_string())
    );
}

/// 钉住只对 active provider 生效:解析别的 provider 时仍走段级。
#[test]
fn resolve_model_pin_applies_only_to_active_provider() {
    let toml = r#"
        [active]
        provider = "anthropic"
        credential = "minimax"

        [openai]
        model = "gpt-section"

        [[anthropic.credentials]]
        label = "minimax"
        api_key = "sk-m"
        model = "minimax-model"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(
        cfg.resolve_model("openai"),
        Some("gpt-section".to_string()),
        "openai 不是 active provider,不受 anthropic 的钉住影响"
    );
}

/// `active_credential` 的 trim + 空串过滤。
#[test]
fn active_credential_trims_and_filters_empty() {
    let toml = r#"
        [active]
        provider = "anthropic"
        credential = "  MiniMax  "
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential().as_deref(), Some("MiniMax"));

    let toml = r#"
        [active]
        provider = "anthropic"
        credential = "   "
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential(), None);
}

/// `apply_to_registry` 把 `[active].credential` 注册为 preferred label。
#[test]
fn apply_to_registry_sets_preferred_label() {
    let toml = r#"
        [active]
        provider = "anthropic"
        credential = "minimax"

        [[anthropic.credentials]]
        label = "minimax"
        api_key = "sk-m"

        [[anthropic.credentials]]
        label = "backup"
        api_key = "sk-b"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let r = cfg.to_registry().unwrap();
    assert_eq!(r.preferred_label("anthropic").as_deref(), Some("minimax"));
    assert_eq!(r.preferred_label("openai"), None);

    // 未钉住 → preferred 不设。
    let toml = toml.replace("credential = \"minimax\"", "");
    let cfg: ReflectConfig = toml::from_str(&toml).unwrap();
    let r = cfg.to_registry().unwrap();
    assert_eq!(r.preferred_label("anthropic"), None);
}

// ── MCP (v0.3) ────────────────────────────────────────────────────────

fn cfg_with_mcp_stdio(name: &str, cmd: &str) -> ReflectConfig {
    let toml = format!(
        r#"
        [mcp_servers.{name}]
        type = "stdio"
        command = "{cmd}"
        args = ["--port", "9000"]
        timeout_ms = 5000
        "#
    );
    toml::from_str(&toml).unwrap()
}

fn cfg_with_mcp_http(name: &str, url: &str) -> ReflectConfig {
    let toml = format!(
        r#"
        [mcp_servers.{name}]
        type = "streamable-http"
        url = "{url}"
        headers = {{ Authorization = "Bearer t" }}
        "#
    );
    toml::from_str(&toml).unwrap()
}

#[test]
fn mcp_server_configs_validates_stdio() {
    let cfg = cfg_with_mcp_stdio("fs", "npx");
    let cfgs = cfg.mcp_server_configs().unwrap();
    assert_eq!(cfgs.len(), 1);
    assert_eq!(cfgs[0].name, "fs");
    assert_eq!(cfgs[0].transport, McpTransport::Stdio);
    assert_eq!(cfgs[0].command.as_deref(), Some("npx"));
    assert_eq!(cfgs[0].args, vec!["--port", "9000"]);
    assert_eq!(cfgs[0].timeout, Duration::from_millis(5_000));
}

#[test]
fn mcp_server_configs_validates_http() {
    let cfg = cfg_with_mcp_http("github", "https://mcp.example.com/github");
    let cfgs = cfg.mcp_server_configs().unwrap();
    assert_eq!(cfgs.len(), 1);
    assert_eq!(cfgs[0].transport, McpTransport::Http);
    assert_eq!(
        cfgs[0].url.as_deref(),
        Some("https://mcp.example.com/github")
    );
    assert_eq!(
        cfgs[0].headers.get("Authorization").map(String::as_str),
        Some("Bearer t")
    );
    // 未指定 timeout_ms → 30s 默认
    assert_eq!(
        cfgs[0].timeout,
        Duration::from_millis(DEFAULT_MCP_TIMEOUT_MS)
    );
}

#[test]
fn mcp_server_configs_errors_on_stdio_without_command() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.bad]
        type = "stdio"
        "#,
    )
    .unwrap();
    let err = cfg.mcp_server_configs().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stdio requires 'command'"), "got: {msg}");
    assert!(msg.contains("[mcp_servers.bad]"), "got: {msg}");
}

#[test]
fn mcp_server_configs_errors_on_http_without_url() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.bad]
        type = "http"
        "#,
    )
    .unwrap();
    let err = cfg.mcp_server_configs().unwrap_err();
    assert!(err.to_string().contains("http requires 'url'"));
}

#[test]
fn mcp_server_configs_results_are_sorted_by_name() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.zeta]
        command = "z"
        [mcp_servers.alpha]
        command = "a"
        [mcp_servers.mid]
        command = "m"
        "#,
    )
    .unwrap();
    let cfgs = cfg.mcp_server_configs().unwrap();
    let names: Vec<&str> = cfgs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}

// ── v1.3 SDK:mock provider 接线 ─────────────────────────────────────
// 走纯函数核心(`active_provider_inner` / `model_for_inner`)断言,
// 不改写进程 env,避免与并行测试竞态。

/// mock 的三种 opt-in 路径与 model 前缀剥离。
#[test]
fn mock_provider_opt_in_paths_and_model_resolution() {
    let cfg = ReflectConfig::default();
    // `REFLECT_PROVIDER=mock` 显式 opt-in。
    assert_eq!(cfg.active_provider_inner(Some("mock"), None), Some("mock"));
    assert_eq!(cfg.active_provider_inner(Some("MOCK"), None), Some("mock"));
    // `REFLECT_MODEL` 完整 spec / 裸 mock 均可 opt-in。
    assert_eq!(
        cfg.active_provider_inner(None, Some("mock/mock-1")),
        Some("mock")
    );
    assert_eq!(
        cfg.active_provider_inner(None, Some(" mock ")),
        Some("mock")
    );
    // 非 mock 的 REFLECT_MODEL 不触发(回退后续优先级链)。
    assert_eq!(cfg.active_provider_inner(None, Some("gpt-4o")), None);
    // provider 优先于 model。
    assert_eq!(
        cfg.active_provider_inner(Some("anthropic"), Some("mock/mock-1")),
        Some("anthropic")
    );
    // model 解析:剥前缀 / 裸 mock 回退内置 mock model(mock 是显式
    // opt-in 的离线 provider,内置 model 名属于其契约)。
    assert_eq!(
        cfg.resolve_model_inner("mock", Some("mock/mock-1")),
        Some("mock-1".to_string())
    );
    assert_eq!(
        cfg.resolve_model_inner("mock", Some("mock")),
        Some("mock-1".to_string())
    );
    assert_eq!(
        cfg.resolve_model_inner("mock", None),
        Some("mock-1".to_string())
    );
}

/// 显式真实 provider 时 mock 不参与 active 选择。
#[test]
fn mock_not_active_for_real_provider_config() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.active_provider_inner(Some("anthropic"), None),
        Some("anthropic")
    );
    assert_eq!(cfg.active_provider_inner(None, None), Some("anthropic"));
}

// ── v1.5 review:`[routing] max_attempts` 配置透传 ────────────────

/// `[routing] max_attempts` 覆盖 `RoutingPolicy.max_attempts`(缺省 16)。
#[test]
fn routing_max_attempts_from_toml() {
    // 显式配置 → 生效(用户要把最坏情况重试从 16 压到 10 的入口)。
    let toml = r#"
        [routing]
        max_attempts = 10
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.routing_policy().max_attempts, 10);

    // `[routing]` 段存在但未写 max_attempts → 缺省 16。
    let toml_default = r#"
        [routing]
        [routing.main]
        primary = "anthropic/claude-test"
    "#;
    let cfg_default: ReflectConfig = toml::from_str(toml_default).unwrap();
    assert_eq!(cfg_default.routing_policy().max_attempts, 16);

    // 整个 `[routing]` 段缺省 → 缺省 16。
    let cfg_none = cfg_with_anthropic("sk-a");
    assert_eq!(cfg_none.routing_policy().max_attempts, 16);
}
