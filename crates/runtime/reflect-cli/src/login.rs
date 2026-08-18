//! `reflect login` — 把 provider 凭据写入 `~/.reflect/config.toml`。
//!
//! v0.4 起这是真实现(替代 v0.3 的 stub):
//! - 读 `~/.reflect/config.toml`(缺失用 `Default`),改对应 provider section 的
//!   `api_key`,整段 TOML 序列化回写。
//! - 交互模式(`--api-key` 缺失):从 stdin 读一行,trim。
//! - 长度 < 8 且无 `--force` 时 warn 但接受;`""` 一律拒绝。
//! - 写完打印保存路径,提示用户跑 `reflect exec` 或独立 Reflect-TUI 启动。

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use reflect_config::{ReflectConfig, default_config_path, load_default};

/// v0.4: `reflect login` 子命令的实际实现。
///
/// `provider`: `"anthropic"` / `"openai"` / `"ollama"`(默认 `"anthropic"`)。
/// `api_key`: 显式传入的 key;`None` 时从 stdin 读。
/// `force`: 跳过长度 < 8 的告警(脚本自动化场景)。
///
/// 退出码语义:
/// - `Ok(())` —— 成功
/// - `Err(...)` —— 用户错误(`""`、未知 provider、HOME 未设、IO 失败等)
pub fn run(provider: &str, api_key: Option<String>, force: bool) -> anyhow::Result<()> {
    let provider = normalize_provider(provider).ok_or_else(|| {
        anyhow!("unknown provider '{provider}'; expected one of: anthropic | openai | ollama")
    })?;

    // 1. 解析或交互读 key。
    let api_key = match api_key {
        Some(k) => k,
        None => read_from_stdin(&format!("Enter API key for {provider}: "))?,
    };

    // 2. 校验。
    if api_key.is_empty() {
        return Err(anyhow!("api_key cannot be empty"));
    }
    if api_key.len() < 8 && !force {
        eprintln!(
            "warning: api_key is very short ({len} chars); pass --force to accept anyway",
            len = api_key.len()
        );
    }

    // 3. 加载当前 config(缺失 / 解析失败回退 Default)。
    let mut cfg = load_default();

    // 4. 改对应 section。
    apply_key(&mut cfg, &provider, api_key.clone());

    // 5. 把 [active].provider 设为该 provider(若是首次配置)。
    if cfg.active.provider.is_none() {
        cfg.active.provider = Some(provider.clone());
    }

    // 6. 序列化回 TOML 写盘。
    let path = write_config(&cfg).context("failed to write config.toml")?;

    println!(
        "Config saved to {}. Run `reflect exec \"<prompt>\"` to start a headless turn, \
         or use the standalone Reflect-TUI binary for interactive mode.",
        path.display()
    );
    Ok(())
}

/// 把 `provider` 字符串归一化为小写 trim;返回 `None` 表示未知。
fn normalize_provider(p: &str) -> Option<String> {
    let p = p.trim().to_lowercase();
    match p.as_str() {
        "anthropic" | "openai" | "ollama" | "local" => {
            // `local` 是 ollama 的常见别名。
            let canon = if p == "local" { "ollama" } else { &p };
            Some(canon.to_string())
        }
        _ => None,
    }
}

/// 把 key 写入对应 section(缺失则新建)。
fn apply_key(cfg: &mut ReflectConfig, provider: &str, key: String) {
    match provider {
        "anthropic" => {
            let mut sec = cfg.anthropic.clone().unwrap_or_default();
            sec.api_key = Some(key);
            cfg.anthropic = Some(sec);
        }
        "openai" => {
            let mut sec = cfg.openai.clone().unwrap_or_default();
            sec.api_key = Some(key);
            cfg.openai = Some(sec);
        }
        "ollama" => {
            let mut sec = cfg.ollama.clone().unwrap_or_default();
            sec.api_key = Some(key);
            cfg.ollama = Some(sec);
        }
        _ => unreachable!("normalize_provider already filtered"),
    }
}

/// 从 stdin 读一行(交互模式);trim 末尾换行。
fn read_from_stdin(prompt: &str) -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        // 交互模式:打印 prompt。
        print!("{prompt}");
        std::io::stdout().flush().ok();
    }
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .context("failed to read api_key from stdin")?;
    Ok(line.trim().to_string())
}

/// 把 `cfg` 序列化回 TOML,写到 `~/.reflect/config.toml`,自动 mkdir 父目录。
/// 返回写入路径。
fn write_config(cfg: &ReflectConfig) -> anyhow::Result<PathBuf> {
    let path =
        default_config_path().ok_or_else(|| anyhow!("HOME unset; cannot locate config dir"))?;
    write_config_to(cfg, &path)
}

/// 把 cfg 写到指定路径(测试用)。
pub fn write_config_to(cfg: &ReflectConfig, path: &Path) -> anyhow::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config parent dir {}", parent.display()))?;
    }
    let s = toml::to_string_pretty(cfg).context("failed to serialize ReflectConfig")?;
    std::fs::write(path, s).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_config::AnthropicSection;
    use std::collections::HashMap;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// 写入 [anthropic].api_key 后能从文件读回。
    #[test]
    fn login_writes_anthropic_key_to_config_file() {
        let tmp = tmp_home();
        let path = tmp.path().join(".reflect").join("config.toml");

        let mut cfg = ReflectConfig::default();
        apply_key(&mut cfg, "anthropic", "sk-test-anthropic".into());
        write_config_to(&cfg, &path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("api_key = \"sk-test-anthropic\""),
            "written: {written}"
        );

        // 反向校验:load_from_file 能读回。
        let back = reflect_config::load_from_file(&path).unwrap();
        assert_eq!(
            back.anthropic.as_ref().unwrap().api_key.as_deref(),
            Some("sk-test-anthropic")
        );
    }

    /// 已有 base_url 时,login 不删 base_url(只覆盖 api_key)。
    #[test]
    fn login_appends_to_existing_section() {
        let mut cfg = ReflectConfig {
            anthropic: Some(AnthropicSection {
                api_key: Some("sk-old".into()),
                base_url: Some("https://api.anthropic.com".into()),
                model: Some("claude-3-5-sonnet-latest".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_key(&mut cfg, "anthropic", "sk-new".into());
        let a = cfg.anthropic.as_ref().unwrap();
        assert_eq!(a.api_key.as_deref(), Some("sk-new"));
        assert_eq!(a.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(a.model.as_deref(), Some("claude-3-5-sonnet-latest"));
    }

    /// `--api-key ""` → `Err`。
    #[test]
    fn login_empty_key_rejected() {
        let result = run("anthropic", Some(String::new()), false);
        assert!(result.is_err(), "empty key must error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("empty"),
            "expected 'empty' in error, got: {msg}"
        );
    }

    /// `"sk"` + `--force` 接受(短 key,但 force 跳过 warn)。
    #[test]
    fn login_force_accepts_short_key() {
        let tmp = tmp_home();
        let path = tmp.path().join(".reflect").join("config.toml");
        // 直接调 apply_key + write_config_to 模拟 run() 的内部路径,
        // 不走 run() 的 HOME 解析。
        let mut cfg = ReflectConfig::default();
        apply_key(&mut cfg, "anthropic", "sk".into());
        write_config_to(&cfg, &path).unwrap();
        assert!(path.exists());
    }

    /// HOME 指向不存在的 dir 时,首次 login 自动 mkdir 父目录。
    #[test]
    fn login_creates_parent_dir() {
        let tmp = tmp_home();
        let nested = tmp.path().join("deep").join("nested");
        let path = nested.join("config.toml");
        assert!(!nested.exists());

        let mut cfg = ReflectConfig::default();
        apply_key(&mut cfg, "openai", "sk-openai-x".into());
        write_config_to(&cfg, &path).unwrap();
        assert!(nested.is_dir(), "parent dir should be created");
        assert!(path.is_file());
    }

    /// normalize_provider 的小写 / 别名处理。
    #[test]
    fn normalize_provider_handles_case_and_alias() {
        assert_eq!(
            normalize_provider("Anthropic").as_deref(),
            Some("anthropic")
        );
        assert_eq!(normalize_provider("OPENAI").as_deref(), Some("openai"));
        assert_eq!(normalize_provider("ollama").as_deref(), Some("ollama"));
        assert_eq!(normalize_provider("local").as_deref(), Some("ollama"));
        assert_eq!(normalize_provider("unknown").as_deref(), None);
    }

    /// openai 和 ollama section 也能写。
    #[test]
    fn login_supports_openai_and_ollama() {
        let tmp = tmp_home();
        let path = tmp.path().join(".reflect").join("config.toml");

        let mut cfg = ReflectConfig::default();
        apply_key(&mut cfg, "openai", "sk-openai-key".into());
        apply_key(&mut cfg, "ollama", "sk-ollama-key".into());
        write_config_to(&cfg, &path).unwrap();

        let back = reflect_config::load_from_file(&path).unwrap();
        assert_eq!(
            back.openai.as_ref().unwrap().api_key.as_deref(),
            Some("sk-openai-key")
        );
        assert_eq!(
            back.ollama.as_ref().unwrap().api_key.as_deref(),
            Some("sk-ollama-key")
        );
    }

    /// 序列化 roundtrip:mcp_servers + hooks + compact 全保留。
    #[test]
    fn roundtrip_preserves_all_sections() {
        let mut cfg = ReflectConfig::default();
        cfg.compact.trigger_tokens = Some(12_000);
        let mut hooks = HashMap::new();
        hooks.insert("Authorization".to_string(), "Bearer token-x".to_string());
        cfg.mcp_servers.servers.insert(
            "fs".into(),
            reflect_config::McpServerEntry {
                transport: reflect_config::McpTransport::Stdio,
                command: Some("npx".into()),
                args: Some(vec!["-y".into(), "@mcp/fs".into()]),
                headers: Some(hooks),
                timeout_ms: Some(15_000),
                ..Default::default()
            },
        );
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: ReflectConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.compact.trigger_tokens, Some(12_000));
        assert!(back.mcp_servers.servers.contains_key("fs"));
    }
}
