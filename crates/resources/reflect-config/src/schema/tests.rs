//! `schema` 模块单测。整体迁自原 `schema.rs` 内联 `#[cfg(test)] mod tests`,
//! 逻辑零变化。

use super::*;

// ── 解析基础 ──────────────────────────────────────────────────────────

/// Phase 2:`[hooks]` 块空 → `read_before_edit = None`(默认行为)。
#[test]
fn hooks_section_without_read_before_edit_is_none() {
    let cfg: ReflectConfig = toml::from_str("[hooks]\nenabled = []\n").unwrap();
    assert!(cfg.hooks.read_before_edit.is_none());
}

/// Phase 2:`[hooks.read_before_edit]` 显式块解析为完整 section。
#[test]
fn hooks_section_parses_read_before_edit_with_values() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [hooks.read_before_edit]
        enabled = true
        mtime_drift_tolerance_ms = 1000
        "#,
    )
    .unwrap();
    let rbe = cfg
        .hooks
        .read_before_edit
        .expect("read_before_edit section parsed");
    assert_eq!(rbe.enabled, Some(true));
    assert_eq!(rbe.mtime_drift_tolerance_ms, Some(1000));
}

/// `[web_search]` 段缺失时 → `None`(向后兼容)。
#[test]
fn web_search_section_absent_is_none() {
    let cfg: ReflectConfig = toml::from_str("[active]\nprovider = \"anthropic\"\n").unwrap();
    assert!(cfg.web_search.is_none());
}

/// `[web_search].api_key` 显式配置 → 解析为 `Some`。
#[test]
fn web_search_section_parses_api_key() {
    let cfg: ReflectConfig = toml::from_str("[web_search]\napi_key = \"BSA123\"\n").unwrap();
    let ws = cfg.web_search.expect("web_search section present");
    assert_eq!(ws.api_key.as_deref(), Some("BSA123"));
}

/// Phase 2:`ReadBeforeEditSection` 独立构造 + serde round-trip。
#[test]
fn read_before_edit_section_serde_roundtrip() {
    let s = ReadBeforeEditSection {
        enabled: Some(false),
        mtime_drift_tolerance_ms: Some(250),
    };
    let j = serde_json::to_string(&s).unwrap();
    let back: ReadBeforeEditSection = serde_json::from_str(&j).unwrap();
    assert_eq!(back, s);
}

#[test]
fn parses_stdio_mcp_server_entry() {
    let toml = r#"
        [mcp_servers.filesystem]
        type = "stdio"
        command = "npx"
        args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        timeout_ms = 30000
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let entry = cfg.mcp_servers.servers.get("filesystem").expect("entry");
    assert_eq!(entry.transport, McpTransport::Stdio);
    assert_eq!(entry.command.as_deref(), Some("npx"));
    assert_eq!(
        entry.args.as_deref(),
        Some(
            [
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
                "/tmp".to_string(),
            ]
            .as_slice()
        )
    );
    assert_eq!(entry.timeout_ms, Some(30_000));
}

#[test]
fn parses_http_mcp_server_entry_with_alias() {
    let toml = r#"
        [mcp_servers.github]
        type = "streamable-http"
        url = "https://mcp.example.com/github"
        headers = { Authorization = "Bearer xyz" }
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let entry = cfg.mcp_servers.servers.get("github").expect("entry");
    assert_eq!(entry.transport, McpTransport::Http);
    assert_eq!(entry.url.as_deref(), Some("https://mcp.example.com/github"));
    assert_eq!(
        entry
            .headers
            .as_ref()
            .and_then(|h| h.get("Authorization"))
            .map(String::as_str),
        Some("Bearer xyz")
    );
}

#[test]
fn transport_defaults_to_stdio_when_omitted() {
    let toml = r#"
        [mcp_servers.minimal]
        command = "echo"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let entry = cfg.mcp_servers.servers.get("minimal").unwrap();
    assert_eq!(entry.transport, McpTransport::Stdio);
}

#[test]
fn unknown_keys_are_ignored() {
    // Reflect 的容忍策略:未知字段不报错,留 v0.4 加 strict 模式。
    let toml = r#"
        [mcp_servers.x]
        command = "echo"
        future_field = "ignored"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert!(cfg.mcp_servers.servers.contains_key("x"));
}

// ── PartialEq 比较能力 (给 diff_sections 用) ────────────────────────

#[test]
fn partial_eq_detects_added_server() {
    let old = ReflectConfig::default();
    let new: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.github]
        type = "http"
        url = "https://x"
    "#,
    )
    .unwrap();
    assert_ne!(old.mcp_servers, new.mcp_servers);
}

#[test]
fn partial_eq_detects_removed_server() {
    let old: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "echo"
    "#,
    )
    .unwrap();
    let new = ReflectConfig::default();
    assert_ne!(old.mcp_servers, new.mcp_servers);
}

#[test]
fn partial_eq_detects_command_field_change() {
    let a: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "echo"
    "#,
    )
    .unwrap();
    let b: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "cat"
    "#,
    )
    .unwrap();
    assert_ne!(a.mcp_servers, b.mcp_servers);
}

#[test]
fn partial_eq_detects_timeout_change() {
    // timeout 变化本身不触发 restart,但仍属 config change (reload 路径会
    // diff_sections 列出 "mcp_servers")。此测试锁定 partial_eq 语义。
    let a: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "echo"
        timeout_ms = 30000
    "#,
    )
    .unwrap();
    let b: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "echo"
        timeout_ms = 60000
    "#,
    )
    .unwrap();
    assert_ne!(a.mcp_servers, b.mcp_servers);
}

// ── Ollama 段(v0.3.1)─────────────────────────────────────────

#[test]
fn parses_full_ollama_section() {
    let toml = r#"
        [ollama]
        base_url = "http://192.168.1.5:11434"
        api_key = "sk-local"
        model = "qwen2.5:7b"
        keep_alive_secs = 300
        num_ctx = 8192
        num_gpu = 99
        timeout_secs = 120
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let o = cfg.ollama.as_ref().expect("ollama section");
    assert_eq!(o.base_url.as_deref(), Some("http://192.168.1.5:11434"));
    assert_eq!(o.api_key.as_deref(), Some("sk-local"));
    assert_eq!(o.model.as_deref(), Some("qwen2.5:7b"));
    assert_eq!(o.keep_alive_secs, Some(300));
    assert_eq!(o.num_ctx, Some(8192));
    assert_eq!(o.num_gpu, Some(99));
    assert_eq!(o.timeout_secs, Some(120));
}

#[test]
fn parses_minimal_ollama_section_with_all_none_fields() {
    // 仅声明 `[ollama]`,所有字段都缺省 → 全 None,builder 走兜底。
    let toml = r#"
        [ollama]
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let o = cfg.ollama.as_ref().expect("ollama section");
    assert!(o.base_url.is_none());
    assert!(o.api_key.is_none());
    assert!(o.model.is_none());
    assert!(o.keep_alive_secs.is_none());
    assert!(o.num_ctx.is_none());
    assert!(o.num_gpu.is_none());
    assert!(o.timeout_secs.is_none());
}

#[test]
fn partial_eq_detects_ollama_section_change() {
    // 改 keep_alive_secs → partial_eq 不等 → diff_sections 报 "ollama"。
    let a: ReflectConfig = toml::from_str(
        r#"
        [ollama]
        model = "llama3.2"
        keep_alive_secs = 300
        "#,
    )
    .unwrap();
    let mut b = a.clone();
    b.ollama.as_mut().unwrap().keep_alive_secs = Some(600);
    assert_ne!(a.ollama, b.ollama);
}

#[test]
fn unknown_ollama_keys_ignored() {
    // 容忍策略:未知字段不报错(v0.4 strict 模式留待)。
    let toml = r#"
        [ollama]
        model = "llama3.2"
        future_field = "ignored"
        "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert!(cfg.ollama.is_some());
}

#[test]
fn partial_eq_unchanged_when_only_other_section_changes() {
    let a: ReflectConfig = toml::from_str(
        r#"
        [mcp_servers.x]
        command = "echo"
        [compact]
        trigger_tokens = 10000
    "#,
    )
    .unwrap();
    let mut b = a.clone();
    b.compact.trigger_tokens = Some(20000);
    assert_eq!(a.mcp_servers, b.mcp_servers);
}

// ── v1.2 P1-12:token 预算段 ──────────────────────────────────────

#[test]
fn token_budget_section_parses_session_total() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [token_budget]
        session_total_tokens = 500000
    "#,
    )
    .unwrap();
    assert_eq!(
        cfg.token_budget.as_ref().unwrap().session_total_tokens,
        Some(500_000)
    );
    // per_turn_input_tokens 未给 → None(可选)。
    assert_eq!(
        cfg.token_budget.as_ref().unwrap().per_turn_input_tokens,
        None
    );
}

#[test]
fn token_budget_section_defaults_to_none_when_absent() {
    let cfg: ReflectConfig = toml::from_str("").unwrap();
    assert!(cfg.token_budget.is_none(), "absent section → None");
}

#[test]
fn token_budget_partial_eq_detects_change() {
    let a: ReflectConfig = toml::from_str(
        r#"
        [token_budget]
        session_total_tokens = 100000
    "#,
    )
    .unwrap();
    let mut b = a.clone();
    assert_eq!(a.token_budget, b.token_budget);
    b.token_budget.as_mut().unwrap().session_total_tokens = Some(200_000);
    assert_ne!(a.token_budget, b.token_budget, "change must be detected");
}

// ── v1.0.0-rc2:插件段 ──────────────────────────────────────────

#[test]
fn plugins_section_defaults_to_empty() {
    let cfg = ReflectConfig::default();
    assert!(cfg.plugins.enabled_plugins.is_empty());
    assert!(cfg.plugins.marketplaces.is_empty());
}

#[test]
fn plugins_section_parses_enabled_and_marketplaces() {
    let toml = r#"
        [plugins]
        enabled_plugins = ["code-formatter@anthropic-tools", "local@inline"]

        [plugins.marketplaces.official]
        type = "github"
        repo = "anthropics/claude-plugins-official"
        auto_update = true

        [plugins.marketplaces.local-dev]
        type = "directory"
        path = "/Users/me/dev/marketplace"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.plugins.enabled_plugins.len(), 2);
    assert!(cfg.plugins.is_enabled("code-formatter@anthropic-tools"));
    assert!(cfg.plugins.is_enabled("local@inline"));
    assert!(!cfg.plugins.is_enabled("ghost@nowhere"));
    let official = cfg.plugins.marketplaces.get("official").unwrap();
    assert_eq!(official.kind, PluginMarketplaceKind::Github);
    assert_eq!(
        official.repo.as_deref(),
        Some("anthropics/claude-plugins-official")
    );
    assert!(official.auto_update);
    let local = cfg.plugins.marketplaces.get("local-dev").unwrap();
    assert_eq!(local.kind, PluginMarketplaceKind::Directory);
    assert_eq!(
        local.path.as_deref(),
        Some(std::path::Path::new("/Users/me/dev/marketplace"))
    );
}

#[test]
fn plugins_partial_eq_detects_changes() {
    let mut a = ReflectConfig::default();
    a.plugins.enabled_plugins.push("foo@bar".into());
    let mut b = ReflectConfig::default();
    b.plugins.enabled_plugins.push("baz@qux".into());
    assert_ne!(a.plugins, b.plugins);
}

#[test]
fn plugins_partial_eq_unchanged_when_only_other_section_changes() {
    let mut a = ReflectConfig::default();
    a.plugins.enabled_plugins.push("foo@bar".into());
    let mut b = a.clone();
    b.compact.trigger_tokens = Some(20000);
    assert_eq!(a.plugins, b.plugins);
}

#[test]
fn plugins_marketplace_kind_defaults_to_directory() {
    let toml = r#"
        [plugins.marketplaces.defaults]
        path = "/tmp/m"
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let m = cfg.plugins.marketplaces.get("defaults").unwrap();
    assert_eq!(m.kind, PluginMarketplaceKind::Directory);
    assert_eq!(m.path.as_deref(), Some(std::path::Path::new("/tmp/m")));
}

// ── v1.1.0 Phase 4:协调器段 ─────────────────────────────────

/// `[coordinator]` 段默认 None,可通过 TOML 启用。
#[test]
fn coordinator_section_defaults_to_none() {
    let cfg = ReflectConfig::default();
    assert!(cfg.coordinator.is_none());
}

/// 解析 `[coordinator]` 段到 `Some(CoordinatorSection)`。
#[test]
fn parses_coordinator_section() {
    let toml = r#"
        [coordinator]
        enabled = true
        system_prompt_path = "/etc/coordinator.md"
        max_workers = 8
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let c = cfg.coordinator.expect("coordinator section");
    assert_eq!(c.enabled, Some(true));
    assert_eq!(
        c.system_prompt_path.as_deref(),
        Some(std::path::Path::new("/etc/coordinator.md"))
    );
    assert_eq!(c.max_workers, Some(8));
}

/// 旧 TOML 无 `[coordinator]` → None,保留向后兼容。
#[test]
fn old_toml_without_coordinator_loads() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [active]
        provider = "anthropic"
    "#,
    )
    .unwrap();
    assert!(cfg.coordinator.is_none());
}

/// partial_eq 检测 `[coordinator]` 段变化。
#[test]
fn coordinator_partial_eq_detects_change() {
    let a = ReflectConfig::default();
    let b = ReflectConfig {
        coordinator: Some(CoordinatorSection {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_ne!(a.coordinator, b.coordinator);
}

// ── v1.0.0-rc2:脱敏段 ─────────────────────────────────────

/// 缺省时 `sanitize` 为 `None`,与 `enabled = true`(运行时默认)一致。
#[test]
fn sanitize_section_defaults_to_none() {
    let cfg = ReflectConfig::default();
    assert!(cfg.sanitize.is_none());
}

/// 解析完整 `[sanitize]` 段,所有字段都正确读取。
#[test]
fn parses_full_sanitize_section() {
    let toml = r#"
        [sanitize]
        enabled = true
        marker = "[HIDDEN]"
        disable_default_patterns = false
        extra_patterns = [
            "(?i)\\bmy_token\\s*=\\s*\\S+",
            "(?i)\\binternal_key\\b",
        ]
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let s = cfg.sanitize.expect("sanitize section");
    assert_eq!(s.enabled, Some(true));
    assert_eq!(s.marker.as_deref(), Some("[HIDDEN]"));
    assert_eq!(s.disable_default_patterns, Some(false));
    let extras = s.extra_patterns.expect("extra_patterns");
    assert_eq!(extras.len(), 2);
    assert!(extras[0].contains("my_token"));
    assert!(extras[1].contains("internal_key"));
}

/// 仅声明 `[sanitize]` 空表,字段全部为 `None`。
#[test]
fn parses_minimal_sanitize_section_with_all_none_fields() {
    let toml = r#"
        [sanitize]
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let s = cfg.sanitize.expect("sanitize section");
    assert!(s.enabled.is_none());
    assert!(s.marker.is_none());
    assert!(s.disable_default_patterns.is_none());
    assert!(s.extra_patterns.is_none());
}

/// 旧 TOML(无 `[sanitize]` 段)依然能解析,向后兼容。
#[test]
fn old_toml_without_sanitize_loads() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [active]
        provider = "anthropic"
    "#,
    )
    .unwrap();
    assert!(cfg.sanitize.is_none());
}

/// TOML round-trip:序列化后再反序列化,字段保持一致。
#[test]
fn sanitize_section_round_trip() {
    let toml = r#"
        [sanitize]
        enabled = false
        marker = "[X]"
        extra_patterns = ["(?i)foo"]
    "#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let serialized = toml::to_string(&cfg).unwrap();
    let cfg2: ReflectConfig = toml::from_str(&serialized).unwrap();
    assert_eq!(cfg.sanitize, cfg2.sanitize);
}

/// partial_eq 检测 `SanitizeSection` 字段变化。
#[test]
fn sanitize_partial_eq_detects_change() {
    let a = SanitizeSection {
        enabled: Some(true),
        ..Default::default()
    };
    let b = SanitizeSection {
        enabled: Some(false),
        ..Default::default()
    };
    assert_ne!(a, b);
}

/// partial_eq 在 marker 变更时也检测得到。
#[test]
fn sanitize_partial_eq_detects_marker_change() {
    let a = SanitizeSection {
        marker: Some("[A]".into()),
        ..Default::default()
    };
    let b = SanitizeSection {
        marker: Some("[B]".into()),
        ..Default::default()
    };
    assert_ne!(a, b);
}

/// ReflectConfig 顶层 partial_eq 在 sanitize 变化时也检测得到。
#[test]
fn reflect_config_partial_eq_detects_sanitize_change() {
    let a = ReflectConfig::default();
    let mut b = a.clone();
    b.sanitize = Some(SanitizeSection {
        enabled: Some(false),
        ..Default::default()
    });
    assert_ne!(a, b);
}

// ── [model] 段(未知模型 metrics 兜底) ───────────────────────────────

/// `[model]` 段缺省 → `None`(零行为变化)。
#[test]
fn model_section_absent_by_default() {
    let cfg: ReflectConfig = toml::from_str("[active]\nprovider = \"anthropic\"\n").unwrap();
    assert!(cfg.model.is_none());
}

/// `[model]` 段解析 context_window + micro-USD 计价。
#[test]
fn model_section_parses_context_window_and_pricing() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [model]
        context_window = 1000000
        input_price_micro_usd_per_mtok = 3000000
        output_price_micro_usd_per_mtok = 15000000
        "#,
    )
    .unwrap();
    let m = cfg.model.expect("[model] section parsed");
    assert_eq!(m.context_window, Some(1_000_000));
    assert_eq!(m.input_price_micro_usd_per_mtok, Some(3_000_000));
    assert_eq!(m.output_price_micro_usd_per_mtok, Some(15_000_000));
    // 换算 helper:$3.00/Mtok 输入,$15.00/Mtok 输出。
    assert!((m.input_price_usd_per_mtok().unwrap() - 3.0).abs() < 1e-9);
    assert!((m.output_price_usd_per_mtok().unwrap() - 15.0).abs() < 1e-9);
}

/// 只配 context_window 不配价格:input_price → None(不自行计价)。
#[test]
fn model_section_context_window_only() {
    let cfg: ReflectConfig = toml::from_str("[model]\ncontext_window = 256000\n").unwrap();
    let m = cfg.model.expect("[model] section parsed");
    assert_eq!(m.context_window, Some(256_000));
    assert!(m.input_price_usd_per_mtok().is_none());
}

/// `[permissions]` 段应正确反序列化为 PermissionsSection(含 shell_pattern)。
#[test]
fn permissions_section_parses_rules() {
    let toml = r#"
[[permissions.rule]]
tool = "Bash"
action = "allow"
shell_pattern = "git *"

[[permissions.rule]]
tool = "Write"
action = "deny"
"#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let sec = cfg.permissions.expect("[permissions] section parsed");
    assert_eq!(sec.rules.len(), 2, "应解析 2 条规则");
    assert_eq!(sec.rules[0].tool, "Bash");
    assert_eq!(
        sec.rules[0].action,
        reflect_permissions::PermissionAction::Allow
    );
    assert_eq!(sec.rules[0].shell_pattern.as_deref(), Some("git *"));
    assert_eq!(sec.rules[1].tool, "Write");
    assert_eq!(
        sec.rules[1].action,
        reflect_permissions::PermissionAction::Deny
    );
}

/// 无 `[permissions]` 段时,字段为 None(向后兼容,行为不变)。
#[test]
fn permissions_section_absent_is_none() {
    let cfg: ReflectConfig = toml::from_str("[active]\nprovider = \"anthropic\"\n").unwrap();
    assert!(cfg.permissions.is_none(), "无 [permissions] 段应为 None");
}

// ── Claude Code 式 allow/deny 紧凑数组 ────────────────────────────────

/// `[permissions] allow` / `deny` 数组应正确反序列化为字符串列表。
#[test]
fn permissions_section_parses_allow_deny_arrays() {
    let toml = r#"
[permissions]
allow = ["Bash", "Edit", "Bash(git:*)"]
deny  = ["Bash(curl:*)"]
"#;
    let cfg: ReflectConfig = toml::from_str(toml).unwrap();
    let sec = cfg.permissions.expect("[permissions] section parsed");
    assert_eq!(sec.allow, vec!["Bash", "Edit", "Bash(git:*)"]);
    assert_eq!(sec.deny, vec!["Bash(curl:*)"]);
    assert!(
        sec.rules.is_empty(),
        "未写 [[permissions.rule]] 时 rules 为空"
    );
}

/// `expanded_rules()` 把 deny/allow 紧凑字符串展平为 PermissionRule,
/// 顺序为 deny → allow → 显式 rules,确保 deny 全局短路优先。
#[test]
fn expanded_rules_flattens_allow_deny_in_correct_order() {
    let sec = PermissionsSection {
        rules: vec![reflect_permissions::PermissionRule {
            tool: "Write".into(),
            action: reflect_permissions::PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: None,
        }],
        allow: vec!["Bash".into(), "Bash(git:*)".into()],
        deny: vec!["Bash(curl:*)".into()],
    };
    let rules = sec.expanded_rules();
    // deny 在前 → allow → 显式 rules。
    assert_eq!(rules.len(), 4);
    // deny 规则置顶(全局短路)。
    assert_eq!(rules[0].action, reflect_permissions::PermissionAction::Deny);
    assert_eq!(rules[0].tool, "Bash");
    assert_eq!(rules[0].shell_pattern.as_deref(), Some("curl*"));
    // allow 紧随。
    assert_eq!(
        rules[1].action,
        reflect_permissions::PermissionAction::Allow
    );
    assert_eq!(rules[1].tool, "Bash");
    assert!(rules[1].shell_pattern.is_none());
    assert_eq!(rules[2].tool, "Bash");
    assert_eq!(rules[2].shell_pattern.as_deref(), Some("git*"));
    // 显式 rules 在末尾。
    assert_eq!(rules[3].tool, "Write");
}

/// allow/deny 字段缺省(向后兼容)时,expanded_rules() 等价于原 rules 克隆。
#[test]
fn expanded_rules_without_arrays_equals_rules() {
    let rule = reflect_permissions::PermissionRule {
        tool: "Read".into(),
        action: reflect_permissions::PermissionAction::Allow,
        tool_glob: None,
        shell_pattern: None,
    };
    let sec = PermissionsSection {
        rules: vec![rule.clone()],
        allow: vec![],
        deny: vec![],
    };
    assert_eq!(sec.expanded_rules(), vec![rule]);
}

/// Claude 式 `Bash(git:*)` 展开后,经 matcher 应对 `git status` 命中 Allow、
/// 对 `curl http://x` 命中 Deny —— 端到端验证解析糖接上底层匹配引擎。
#[test]
fn expanded_rules_end_to_end_with_matcher() {
    use reflect_permissions::evaluate_with_context;
    let sec = PermissionsSection {
        rules: vec![],
        allow: vec!["Bash(git:*)".into()],
        deny: vec!["Bash(curl:*)".into()],
    };
    let rules = sec.expanded_rules();
    assert_eq!(
        evaluate_with_context(&rules, "Bash", Some("git status")),
        reflect_permissions::RuleMatch::Allow
    );
    assert_eq!(
        evaluate_with_context(&rules, "Bash", Some("git diff README.md")),
        reflect_permissions::RuleMatch::Allow
    );
    // curl 被 deny 短路(即便单独看命令也会命中 deny 规则)。
    assert_eq!(
        evaluate_with_context(&rules, "Bash", Some("curl http://evil.example")),
        reflect_permissions::RuleMatch::Deny
    );
    // 未匹配的命令仍 NoMatch(交回默认审批流程)。
    assert_eq!(
        evaluate_with_context(&rules, "Bash", Some("rm -rf /tmp/x")),
        reflect_permissions::RuleMatch::NoMatch
    );
}

// ── [context_windows] per-model 覆盖表 ───────────────────────────────

/// `[context_windows]` 段直接解析为 `HashMap<String, u32>`:顶层 key/value
/// 形式(TOML 友好,无需 `[context_windows.entries]` 嵌套)。
#[test]
fn parses_context_windows_flat_keys() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [context_windows]
        "MiniMax-M3" = 1000000
        "qwen36-1m"  = 1000000
        "abab6.5"    = 245000
        "#,
    )
    .unwrap();
    let cw = cfg
        .context_windows
        .as_ref()
        .expect("context_windows section present");
    assert_eq!(cw.entries.len(), 3);
    assert_eq!(cw.entries.get("MiniMax-M3"), Some(&1_000_000));
    assert_eq!(cw.entries.get("qwen36-1m"), Some(&1_000_000));
    assert_eq!(cw.entries.get("abab6.5"), Some(&245_000));
}

/// 备选语法:嵌套 `[context_windows.entries]` 子段也支持(为向后兼容既存手动构造)。
#[test]
fn parses_context_windows_nested_entries() {
    let cfg: ReflectConfig = toml::from_str(
        r#"
        [context_windows.entries]
        "MiniMax-M3" = 1000000
        "#,
    )
    .unwrap();
    let cw = cfg.context_windows.expect("present");
    assert_eq!(cw.entries.get("MiniMax-M3"), Some(&1_000_000));
}

/// 无 `[context_windows]` 段时为 None(向后兼容,行为不变)。
#[test]
fn context_windows_section_absent_is_none() {
    let cfg: ReflectConfig = toml::from_str("[active]\nprovider = \"anthropic\"\n").unwrap();
    assert!(cfg.context_windows.is_none());
}

/// `lookup` 归一化后精确匹配:`MiniMax-M3` → 去前缀/小写 → `minimax-m3`。
#[test]
fn context_windows_lookup_normalizes() {
    let mut entries = HashMap::new();
    entries.insert("minimax-m3".to_string(), 1_000_000);
    let cw = ContextWindowsSection { entries };

    // provider/大小写差异都命中。
    assert_eq!(cw.lookup("MiniMax-M3"), Some(1_000_000));
    assert_eq!(cw.lookup("minimax/MiniMax-M3"), Some(1_000_000));
    assert_eq!(cw.lookup("minimax-m3-latest"), Some(1_000_000)); // 前缀匹配
    assert_eq!(cw.lookup("unknown-model"), None);
}
