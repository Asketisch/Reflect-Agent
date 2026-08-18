//! 运行时配置构造器:tracing 初始化、telemetry sink、quota tracker、
//! sanitizer、coordinator 启用。
//!
//! 这些都是纯配置整形辅助函数,从 `lib.rs` 的 `async_main` 启动路径
//! 原样抽出。

use std::sync::Arc;

use reflect_core::config::M4Deps;
use reflect_llm::SharedQuotaTracker;
use reflect_subagent::SubAgentFactory;
use reflect_task::coordinator::{CoordinatorConfig, build_scratchpad_path, ensure_scratchpad};
use reflect_tools::{SanitizeConfig, Sanitizer, ToolRegistry};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

static OTEL_GUARD: std::sync::OnceLock<reflect_config::ExporterGuard> = std::sync::OnceLock::new();

/// v1.3: 若 `[analytics] enabled = true`,先 `init_exporter` 设置全局
/// TracerProvider,再把 `OpenTelemetryLayer` 加入 registry,这样业务侧
/// `#[tracing::instrument]` / `tracing::info_span!` 自动转 OTLP span。
pub(crate) fn init_tracing(analytics: Option<&reflect_config::AnalyticsSection>) {
    // 启动 OTLP exporter;失败则降级(仅 init_tracing 内 warn,agent 不阻塞)。
    let otel_guard = analytics.and_then(reflect_config::init_exporter);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,reflect=info"));
    let fmt_layer = fmt::layer().with_writer(std::io::stderr);
    // 仅在 OTLP 启用时挂 OpenTelemetryLayer,避免空 subscriber 增加开销。
    let otel_layer = otel_guard
        .as_ref()
        .map(|g| tracing_opentelemetry::layer().with_tracer(g.tracer("reflect")));

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();

    // 把 guard 泄漏到进程全局,确保 agent 主循环退出前不会 drop。
    if let Some(g) = otel_guard {
        let _ = OTEL_GUARD.set(g);
    }
}

/// v1.2 P1:从 `[telemetry]` 配置构造本地 Langfuse 式日志 sink。
///
/// - `enabled = false`(显式)→ 返回 `None`(关闭)。
/// - `enabled = true` 或缺省 → 构造 `TelemetrySink`,trace 目录从
///   `[telemetry].dir` 解析(默认 `~/.reflect/traces`)。
///
/// sink 的 session_id 用一个新生成的 uuid(与 rollout 的 thread_id 独立,
/// 因 telemetry 的 model-io 文件按这个 id 命名)。
pub(crate) fn build_telemetry_sink(
    cfg: &reflect_config::ReflectConfig,
) -> Option<Arc<reflect_telemetry::TelemetrySink>> {
    let default_section = reflect_config::TelemetrySection::default();
    let section = cfg.telemetry.as_ref().unwrap_or(&default_section);
    if !section.is_enabled() {
        return None;
    }
    let base_dir = section
        .dir
        .clone()
        .unwrap_or_else(|| reflect_telemetry::resolve_traces_dir(None));
    let session_id = uuid::Uuid::new_v4().to_string();
    let sink = reflect_telemetry::TelemetrySink::new(base_dir, session_id);

    // P2 `langfuse`:若 [langfuse] 配了 endpoint + public/secret key,启动
    // cloud HTTP exporter 并挂到 sink(本地 JSONL + Langfuse 并行投递)。
    // 缺省 / 缺 key 时 no-op(仅本地日志,向后兼容)。
    if let Some(lf) = cfg.hooks.langfuse_tracker.as_ref() {
        if lf.enabled.unwrap_or(false) {
            if let (Some(endpoint), Some(pk), Some(sk)) = (
                lf.endpoint.as_ref(),
                lf.public_key.as_ref(),
                lf.secret_key.as_ref(),
            ) {
                if !endpoint.is_empty() && !pk.is_empty() && !sk.is_empty() {
                    let lf_cfg = reflect_telemetry::LangfuseConfig {
                        endpoint: endpoint.clone(),
                        public_key: pk.clone(),
                        secret_key: sk.clone(),
                        batch_size: 64,
                    };
                    let exporter = reflect_telemetry::LangfuseExporter::start(Some(lf_cfg));
                    sink.set_langfuse(exporter);
                    tracing::info!(endpoint = %endpoint, "langfuse cloud exporter 已启用");
                }
            }
        }
    }

    Some(sink)
}

/// v1.x 功能 7:遍历 config 所有 provider(anthropic/openai/ollama)的
/// credentials,收集声明了 `quota` 的条目,注册到 `QuotaTracker`。
/// 任一 provider 有 quota 声明 → 返回 `Some(tracker)`;全无 → `None`(向后兼容)。
/// v1 暂忽略 subagent_providers(subagent 配额切换后续迭代)。
pub(crate) fn build_quota_tracker(
    cfg: &reflect_config::ReflectConfig,
) -> Option<SharedQuotaTracker> {
    use reflect_config::QuotaSource;
    use reflect_llm::{
        KimiQuotaProvider, MinimaxQuotaProvider, ZenmuxQuotaProvider, ZhipuQuotaProvider,
    };
    use std::sync::Arc as StdArc;

    let tracker = Arc::new(reflect_llm::QuotaTracker::new());
    let mut any = false;

    // 把 `reflect_config::QuotaSource` 映射为运行时 `QuotaProvider` 实例。
    // 返回 `None` 表示该厂商暂未实现(火山/Anthropic/OpenAI)。
    fn make_provider(src: &QuotaSource) -> Option<Arc<dyn reflect_llm::QuotaProvider>> {
        match src {
            QuotaSource::Kimi => {
                Some(StdArc::new(KimiQuotaProvider) as Arc<dyn reflect_llm::QuotaProvider>)
            }
            QuotaSource::Zhipu => {
                Some(StdArc::new(ZhipuQuotaProvider) as Arc<dyn reflect_llm::QuotaProvider>)
            }
            QuotaSource::Minimax => {
                Some(StdArc::new(MinimaxQuotaProvider) as Arc<dyn reflect_llm::QuotaProvider>)
            }
            QuotaSource::Zenmux => {
                Some(StdArc::new(ZenmuxQuotaProvider) as Arc<dyn reflect_llm::QuotaProvider>)
            }
            // 火山需 AK/SK 签名;Anthropic/OpenAI 需 OAuth 凭据,
            // reflect-agent 用 API Key 不适用,留 TODO(回退本地统计)。
            QuotaSource::Volcengine | QuotaSource::AnthropicUsage | QuotaSource::OpenAIUsage => {
                None
            }
        }
    }

    // 闭包:注册单 provider 的 credentials。
    let mut register_provider = |provider: &str, creds: &[reflect_config::CredentialConfig]| {
        for c in creds {
            if let Some(q) = &c.quota {
                let rt_source = q.check_via.as_ref().map(|s| match s {
                    QuotaSource::Kimi => reflect_llm::QuotaSource::Kimi,
                    QuotaSource::Zhipu => reflect_llm::QuotaSource::Zhipu,
                    QuotaSource::Minimax => reflect_llm::QuotaSource::Minimax,
                    QuotaSource::Zenmux => reflect_llm::QuotaSource::Zenmux,
                    QuotaSource::Volcengine => reflect_llm::QuotaSource::Volcengine,
                    QuotaSource::AnthropicUsage => reflect_llm::QuotaSource::AnthropicUsage,
                    QuotaSource::OpenAIUsage => reflect_llm::QuotaSource::OpenAIUsage,
                });
                let rt = reflect_llm::QuotaConfig {
                    window_secs: q.window_secs,
                    max_tokens: q.max_tokens,
                    check_via: rt_source,
                };
                tracker.register(provider, &c.label, rt);
                // 注册 credential 的 base_url + api_key(厂商 API 查询需要)。
                let base_url = c
                    .base_url
                    .clone()
                    .unwrap_or_else(|| format!("https://{provider}"));
                tracker.register_credential(provider, &c.label, &base_url, &c.api_key);
                // 注入厂商 provider 实例(check_via = Some 时)。
                if let Some(src) = &q.check_via {
                    if let Some(p) = make_provider(src) {
                        tracker.register_provider(provider, &c.label, p);
                    }
                }
                tracing::info!(
                    provider,
                    label = %c.label,
                    window_secs = q.window_secs,
                    max_tokens = q.max_tokens,
                    has_api = q.check_via.is_some(),
                    "registered token plan quota for credential"
                );
                any = true;
            }
        }
    };
    if let Some(s) = &cfg.anthropic {
        register_provider("anthropic", &s.credentials);
    }
    if let Some(s) = &cfg.openai {
        register_provider("openai", &s.credentials);
    }
    if let Some(s) = &cfg.ollama {
        register_provider("ollama", &s.credentials);
    }
    if any { Some(tracker) } else { None }
}

/// v1.0.0-rc2 review 2026-06-30 P0-1:把 `~/.reflect/config.toml [sanitize]`
/// 段真正接到 queue 的脱敏 pass 上 —— 历史实现硬编码
/// `Sanitizer::with_defaults()`,用户的 `enabled = false` / `marker` /
/// `extra_patterns` 全部死信。
///
/// 从 [`reflect_config::SanitizeSection`] 字段映射到
/// `reflect_tools::sanitize::SanitizeConfig`(两个独立 struct,避免
/// `reflect-tools → reflect-config` 依赖环,详见
/// `reflect-config/src/schema.rs:527-555` 的注释)。
///
/// 失败语义:用户写错的 `extra_patterns[i]`(regex 编译失败)→ warn 后
/// fallback 到默认 10-pattern,不阻塞 agent 启动。startup 不能因为
/// 单个自定义 pattern 写错而崩溃,但用户能看到日志。
pub(crate) fn build_sanitizer(section: Option<&reflect_config::SanitizeSection>) -> Arc<Sanitizer> {
    let cfg = SanitizeConfig {
        enabled: section.and_then(|s| s.enabled),
        marker: section.and_then(|s| s.marker.clone()),
        disable_default_patterns: section.and_then(|s| s.disable_default_patterns),
        extra_patterns: section.and_then(|s| s.extra_patterns.clone()),
    };
    match Sanitizer::from_config(&cfg) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "[sanitize] 配置解析失败,fallback 到默认 10-pattern"
            );
            Arc::new(Sanitizer::with_defaults())
        }
    }
}

// ── v1.1.0 Phase 4: coordinator 启停 ─────────────────────────────────────

/// 根据 `[coordinator]` 配置启用/关闭 coordinator 模式。
/// `async_main` 启动与 `handle_reload` 热重载共用此入口。
pub(crate) fn apply_coordinator_from_config(
    cfg: &reflect_config::ReflectConfig,
    workspace: &std::path::Path,
    m4: Option<&M4Deps>,
    factory: &SubAgentFactory,
    tools: &ToolRegistry,
) {
    let default_section = reflect_config::CoordinatorSection::default();
    let section = cfg.coordinator.as_ref().unwrap_or(&default_section);
    let coord_cfg = CoordinatorConfig::from_env_or_config(section);

    if let Some(m4) = m4 {
        let mut pb = m4.prompt_builder.lock();
        if coord_cfg.enabled {
            pb.upsert_section("Coordinator", coord_cfg.system_prompt.clone());
        } else {
            pb.remove_section("Coordinator");
        }
    }

    if coord_cfg.enabled {
        let thread_id = factory.parent_session_id();
        ensure_scratchpad(&coord_cfg, workspace, &thread_id.to_string());
        let scratchpad = build_scratchpad_path(workspace, &thread_id.to_string());
        let footer: String = coordinator_footer(&coord_cfg.system_prompt);
        factory.set_coordinator_mode(true, Some(footer));
        // P2 `git-worktree-auto`:coordinator 启用时,解析 git root 并注入
        // WorktreeCoordinator,让每个 worker spawn 自动隔离到独立 worktree。
        // 非 git 仓库(git_root 失败)仅 warn 跳过,不阻断 coordinator。
        match reflect_tools::git_root(std::path::Path::new(workspace)) {
            Ok(root) => {
                let coord = Arc::new(reflect_tools::WorktreeCoordinator::new(root));
                factory.set_worktree_coordinator(Some(coord));
                tracing::info!("coordinator worktree 隔离已启用");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "coordinator worktree 隔离跳过(非 git 仓库或 git_root 解析失败)"
                );
            }
        }
        tools.register_with_source(
            reflect_tools::ToolSource::Builtin,
            Arc::new(reflect_task::tools::WriteNoteTool::new(Arc::new(
                scratchpad.clone(),
            ))),
        );
        tools.register_with_source(
            reflect_tools::ToolSource::Builtin,
            Arc::new(reflect_task::tools::ReadNotesTool::new(Arc::new(
                scratchpad.clone(),
            ))),
        );
        tracing::info!(
            scratchpad = %scratchpad.display(),
            max_workers = coord_cfg.max_workers,
            "coordinator mode active"
        );
    } else {
        factory.set_coordinator_mode(false, None);
        tools.unregister("WriteNote");
        tools.unregister("ReadNotes");
        tracing::info!("coordinator mode disabled");
    }
}

/// 取 coordinator system_prompt 的末尾 200 个**字符**作为 footer。
///
/// 此前实现是 `prompt[prompt.len()-200..]` —— 字节切片。当 prompt 含多字节
/// 字符(本项目 prompt 几乎都是中文,且默认 coordinator prompt 本身就是中文)
/// 且第 200 个字节落在某个 codepoint 中间时,会触发
/// `byte index is not a char boundary` panic,直接打死 coordinator 启动 /
/// 热重载。这里改按字符边界截取,并抽成独立 fn 便于单测。
fn coordinator_footer(prompt: &str) -> String {
    prompt
        .chars()
        .rev()
        .take(200)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_footer_handles_multibyte_without_panic() {
        // 回归:中文 prompt 长度 > 200 字节,旧字节切片实现会 panic。
        // 这里构造 300 个中文字符(每个 3 字节 = 900 字节),确保末尾
        // 200 字节起点落在 codepoint 中间。
        let prompt: String = "中".repeat(300);
        let footer = coordinator_footer(&prompt);
        // 应取末尾 200 个字符。
        assert_eq!(footer.chars().count(), 200);
        assert!(footer.chars().all(|c| c == '中'));
    }

    #[test]
    fn coordinator_footer_short_prompt_returned_verbatim() {
        // 短于 200 字符的 prompt 原样返回(不补、不截)。
        let prompt = "你好,这是一个很短的 coordinator prompt。";
        let footer = coordinator_footer(prompt);
        assert_eq!(footer, prompt);
    }

    #[test]
    fn coordinator_footer_preserves_order() {
        // 末尾字符顺序必须保持(非逆序)。用单字符标记避免「行N」长短不一
        // 干扰 take(200) 的字符计数。
        let mut prompt = String::new();
        for i in 0..250u8 {
            prompt.push((b'a' + (i % 26)) as char); // a..z 循环
            prompt.push(char::from_digit((i / 26) as u32, 10).unwrap_or('0'));
        }
        let footer = coordinator_footer(&prompt);
        // footer 必须是 prompt 的真后缀(顺序一致),且恰为 200 字符。
        assert!(footer.chars().count() == 200);
        assert!(
            prompt.ends_with(&footer),
            "footer must be an ordered suffix of the prompt"
        );
    }
}
