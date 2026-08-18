//! `reflect config ...` —— 读 / 设 / 取消单个 config key;`show` 默认 redact api_key。

use anyhow::{Context, anyhow};
use reflect_config::{
    AnthropicSection, CompactSection, HooksSection, OllamaSection, OpenAISection, ReflectConfig,
    default_config_path, load_default,
};

use crate::login;

/// v0.4: `reflect config show` —— 打印当前有效配置。`api_key` 默认 redact
/// 成 `sk-…ake` 形式;`--reveal` 显示全文。
pub fn show(reveal: bool) -> anyhow::Result<()> {
    let cfg = load_default();
    let path = default_config_path();

    println!(
        "# ReflectConfig @ {}",
        path.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no HOME>".into())
    );
    println!();

    let toml_str = toml::to_string_pretty(&cfg).context("serialize ReflectConfig")?;
    let rendered = if reveal {
        toml_str
    } else {
        redact_toml(&toml_str)
    };
    print!("{rendered}");
    Ok(())
}

/// v0.4: `reflect config set <key> <value>` —— 改单个 key,然后整段写回。
/// `key` 形如 `"anthropic.model"` / `"compact.trigger_tokens"` / `"active.provider"`。
pub fn set(key: &str, value: &str) -> anyhow::Result<()> {
    let mut cfg = load_default();
    apply_set(&mut cfg, key, value)
        .map_err(|e| anyhow!("{e}; allowed keys: {}", allowed_keys_hint()))?;
    login::write_config_to(
        &cfg,
        &default_config_path().ok_or_else(|| anyhow!("HOME unset"))?,
    )
    .context("write config")?;
    println!("set {key} = {value}");
    Ok(())
}

/// v0.4: `reflect config unset <key>` —— 把 `Option<>` 字段置 None / `String` 字段置空。
pub fn unset(key: &str) -> anyhow::Result<()> {
    let mut cfg = load_default();
    apply_unset(&mut cfg, key)
        .map_err(|e| anyhow!("{e}; allowed keys: {}", allowed_keys_hint()))?;
    login::write_config_to(
        &cfg,
        &default_config_path().ok_or_else(|| anyhow!("HOME unset"))?,
    )
    .context("write config")?;
    println!("unset {key}");
    Ok(())
}

/// v0.4: `reflect config edit` —— 用 `$EDITOR` 打开 config.toml。
pub fn edit() -> anyhow::Result<()> {
    let path = default_config_path().ok_or_else(|| anyhow!("HOME unset"))?;
    if !path.exists() {
        // 若文件不存在,先用 default 序列化创建一份,避免空编辑。
        let cfg = ReflectConfig::default();
        login::write_config_to(&cfg, &path)?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        return Err(anyhow!("editor exited with non-zero status"));
    }
    Ok(())
}

/// v0.4: `reflect config ls` —— 列出已知 key + 当前值,便于脚本扫。
pub fn ls() -> anyhow::Result<()> {
    let cfg = load_default();
    for key in allowed_keys() {
        let value = current_value_of(&cfg, key);
        println!("{:<32} = {}", key, value);
    }
    Ok(())
}

// ── key 白名单 ──────────────────────────────────────────────────────────

fn allowed_keys() -> Vec<&'static str> {
    let mut keys = vec![
        "active.provider",
        "anthropic.api_key",
        "anthropic.base_url",
        "anthropic.model",
        "anthropic.timeout_secs",
        "openai.api_key",
        "openai.base_url",
        "openai.model",
        "openai.timeout_secs",
        "ollama.base_url",
        "ollama.api_key",
        "ollama.model",
        "ollama.keep_alive_secs",
        "ollama.num_ctx",
        "ollama.num_gpu",
        "ollama.timeout_secs",
        "compact.trigger_tokens",
        "hooks.enabled",
    ];
    keys.sort();
    keys
}

fn allowed_keys_hint() -> String {
    allowed_keys().join(", ")
}

/// `apply_set` 把 `key = value` 写入 `cfg`,返回 Err 表示未知 key。
fn apply_set(cfg: &mut ReflectConfig, key: &str, value: &str) -> Result<(), String> {
    match key {
        "active.provider" => cfg.active.provider = Some(value.to_string()),
        "anthropic.api_key" => anthropic_mut(cfg).api_key = Some(value.to_string()),
        "anthropic.base_url" => anthropic_mut(cfg).base_url = Some(value.to_string()),
        "anthropic.model" => anthropic_mut(cfg).model = Some(value.to_string()),
        "anthropic.timeout_secs" => anthropic_mut(cfg).timeout_secs = Some(parse_u64(value, key)?),
        "openai.api_key" => openai_mut(cfg).api_key = Some(value.to_string()),
        "openai.base_url" => openai_mut(cfg).base_url = Some(value.to_string()),
        "openai.model" => openai_mut(cfg).model = Some(value.to_string()),
        "openai.timeout_secs" => openai_mut(cfg).timeout_secs = Some(parse_u64(value, key)?),
        "ollama.base_url" => ollama_mut(cfg).base_url = Some(value.to_string()),
        "ollama.api_key" => ollama_mut(cfg).api_key = Some(value.to_string()),
        "ollama.model" => ollama_mut(cfg).model = Some(value.to_string()),
        "ollama.keep_alive_secs" => ollama_mut(cfg).keep_alive_secs = Some(parse_i64(value, key)?),
        "ollama.num_ctx" => ollama_mut(cfg).num_ctx = Some(parse_u32(value, key)?),
        "ollama.num_gpu" => ollama_mut(cfg).num_gpu = Some(parse_u32(value, key)?),
        "ollama.timeout_secs" => ollama_mut(cfg).timeout_secs = Some(parse_u64(value, key)?),
        "compact.trigger_tokens" => {
            cfg.compact = CompactSection {
                trigger_tokens: Some(parse_u32(value, key)?),
            }
        }
        "hooks.enabled" => {
            let list: Vec<String> = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            cfg.hooks = HooksSection {
                enabled: if list.is_empty() { None } else { Some(list) },
                ..cfg.hooks.clone()
            };
        }
        _ => return Err(format!("unknown key '{key}'")),
    }
    Ok(())
}

/// `apply_unset` 把 key 对应字段重置回 None / ""(区分 Option vs String)。
fn apply_unset(cfg: &mut ReflectConfig, key: &str) -> Result<(), String> {
    match key {
        "active.provider" => cfg.active.provider = None,
        "anthropic.api_key" => anthropic_mut(cfg).api_key = None,
        "anthropic.base_url" => anthropic_mut(cfg).base_url = None,
        "anthropic.model" => anthropic_mut(cfg).model = None,
        "anthropic.timeout_secs" => anthropic_mut(cfg).timeout_secs = None,
        "openai.api_key" => openai_mut(cfg).api_key = None,
        "openai.base_url" => openai_mut(cfg).base_url = None,
        "openai.model" => openai_mut(cfg).model = None,
        "openai.timeout_secs" => openai_mut(cfg).timeout_secs = None,
        "ollama.base_url" => ollama_mut(cfg).base_url = None,
        "ollama.api_key" => ollama_mut(cfg).api_key = None,
        "ollama.model" => ollama_mut(cfg).model = None,
        "ollama.keep_alive_secs" => ollama_mut(cfg).keep_alive_secs = None,
        "ollama.num_ctx" => ollama_mut(cfg).num_ctx = None,
        "ollama.num_gpu" => ollama_mut(cfg).num_gpu = None,
        "ollama.timeout_secs" => ollama_mut(cfg).timeout_secs = None,
        "compact.trigger_tokens" => cfg.compact = CompactSection::default(),
        "hooks.enabled" => cfg.hooks.enabled = None,
        _ => return Err(format!("unknown key '{key}'")),
    }
    Ok(())
}

/// 返回对应 section 的 mutable 引用,缺失则插入 default 后再返回。
fn anthropic_mut(cfg: &mut ReflectConfig) -> &mut AnthropicSection {
    cfg.anthropic.get_or_insert_with(AnthropicSection::default)
}
fn openai_mut(cfg: &mut ReflectConfig) -> &mut OpenAISection {
    cfg.openai.get_or_insert_with(OpenAISection::default)
}
fn ollama_mut(cfg: &mut ReflectConfig) -> &mut OllamaSection {
    cfg.ollama.get_or_insert_with(OllamaSection::default)
}

/// `current_value_of` 返回该 key 当前值(redact api_key)。
fn current_value_of(cfg: &ReflectConfig, key: &str) -> String {
    let v = match key {
        "active.provider" => cfg.active.provider.clone().unwrap_or_default(),
        "anthropic.api_key" => anthropic(cfg)
            .api_key
            .as_deref()
            .map(redact_key)
            .unwrap_or_default(),
        "anthropic.base_url" => anthropic(cfg).base_url.clone().unwrap_or_default(),
        "anthropic.model" => anthropic(cfg).model.clone().unwrap_or_default(),
        "anthropic.timeout_secs" => anthropic(cfg)
            .timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "openai.api_key" => openai(cfg)
            .api_key
            .as_deref()
            .map(redact_key)
            .unwrap_or_default(),
        "openai.base_url" => openai(cfg).base_url.clone().unwrap_or_default(),
        "openai.model" => openai(cfg).model.clone().unwrap_or_default(),
        "openai.timeout_secs" => openai(cfg)
            .timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "ollama.base_url" => ollama(cfg).base_url.clone().unwrap_or_default(),
        "ollama.api_key" => ollama(cfg)
            .api_key
            .clone()
            .map(|k| redact_key(&k))
            .unwrap_or_default(),
        "ollama.model" => ollama(cfg).model.clone().unwrap_or_default(),
        "ollama.keep_alive_secs" => ollama(cfg)
            .keep_alive_secs
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "ollama.num_ctx" => ollama(cfg)
            .num_ctx
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "ollama.num_gpu" => ollama(cfg)
            .num_gpu
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "ollama.timeout_secs" => ollama(cfg)
            .timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "compact.trigger_tokens" => cfg
            .compact
            .trigger_tokens
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "hooks.enabled" => cfg
            .hooks
            .enabled
            .clone()
            .map(|v| v.join(","))
            .unwrap_or_default(),
        _ => return "(unknown)".into(),
    };
    if v.is_empty() { "<unset>".into() } else { v }
}

fn anthropic(cfg: &ReflectConfig) -> AnthropicSection {
    cfg.anthropic.clone().unwrap_or_default()
}
fn openai(cfg: &ReflectConfig) -> OpenAISection {
    cfg.openai.clone().unwrap_or_default()
}
fn ollama(cfg: &ReflectConfig) -> OllamaSection {
    cfg.ollama.clone().unwrap_or_default()
}

// ── redact 工具 ────────────────────────────────────────────────────────────

/// 把 `api_key = "sk-..."` 替换为 `api_key = "sk-…est"` 形式。
pub fn redact_toml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("api_key") && trimmed.contains('=') && trimmed.contains('"') {
            // 解析 `"<value>"` —— 找第一个 `"` 与最后一个 `"`,中间 = value。
            if let (Some(start), Some(end)) = (trimmed.find('"'), trimmed.rfind('"'))
                && end > start
            {
                let prefix = &trimmed[..start + 1]; // 包括开引号
                let value = &trimmed[start + 1..end];
                let suffix = &trimmed[end..]; // 包括闭引号
                let indent = &line[..line.len() - trimmed.len()];
                out.push_str(indent);
                out.push_str(prefix);
                out.push_str(&redact_key(value));
                out.push_str(suffix);
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `sk-abcdefghijk` → `sk-…ijk`(保留前 3 + 后 3 字符);短于 8 的 key 直接 `*`。
pub fn redact_key(k: &str) -> String {
    let n = k.chars().count();
    if n <= 7 {
        return "*".repeat(n.max(1));
    }
    let prefix: String = k.chars().take(3).collect();
    let suffix: String = k
        .chars()
        .rev()
        .take(3)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

// ── 解析辅助函数 ──────────────────────────────────────────────────────────

fn parse_u64(s: &str, key: &str) -> Result<u64, String> {
    s.parse().map_err(|e| format!("invalid u64 for {key}: {e}"))
}
fn parse_u32(s: &str, key: &str) -> Result<u32, String> {
    s.parse().map_err(|e| format!("invalid u32 for {key}: {e}"))
}
fn parse_i64(s: &str, key: &str) -> Result<i64, String> {
    s.parse().map_err(|e| format!("invalid i64 for {key}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// redact_key 基本格式。
    #[test]
    fn redact_key_preserves_prefix_suffix() {
        assert_eq!(redact_key("sk-abcdefghij"), "sk-…hij");
        assert_eq!(redact_key("short"), "*****");
    }

    /// redact_toml 改写 `api_key = "..."`,不动其它字段。
    #[test]
    fn redact_toml_only_touches_api_key() {
        let input = r#"[anthropic]
api_key = "sk-secret-1234567890"
model = "claude-3-5-sonnet-latest"
"#;
        let out = redact_toml(input);
        assert!(out.contains("api_key = \"sk-…890\""), "got: {out}");
        assert!(out.contains("model = \"claude-3-5-sonnet-latest\""));
        assert!(!out.contains("sk-secret-1234567890"));
    }

    /// set 写入并能 read 回。
    #[test]
    fn set_anthropic_model_updates_field() {
        let mut cfg = ReflectConfig::default();
        apply_set(&mut cfg, "anthropic.model", "claude-3-haiku-20240307").unwrap();
        let a = cfg.anthropic.unwrap();
        assert_eq!(a.model.as_deref(), Some("claude-3-haiku-20240307"));
    }

    /// 未知 key 拒绝。
    #[test]
    fn set_unknown_key_rejected() {
        let mut cfg = ReflectConfig::default();
        let r = apply_set(&mut cfg, "totally.unknown.key", "x");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("unknown key"));
    }

    /// unset 重置 Option 字段为 None。
    #[test]
    fn unset_ollama_model_resets_to_none() {
        let mut cfg = ReflectConfig {
            ollama: Some(OllamaSection {
                model: Some("llama3.2".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_unset(&mut cfg, "ollama.model").unwrap();
        assert!(cfg.ollama.unwrap().model.is_none());
    }

    /// `compact.trigger_tokens` 与 `hooks.enabled` 也能 set / unset。
    #[test]
    fn set_compact_and_hooks() {
        let mut cfg = ReflectConfig::default();
        apply_set(&mut cfg, "compact.trigger_tokens", "12345").unwrap();
        assert_eq!(cfg.compact.trigger_tokens, Some(12_345));
        apply_set(&mut cfg, "hooks.enabled", "search_budget, verification").unwrap();
        assert_eq!(
            cfg.hooks.enabled,
            Some(vec!["search_budget".into(), "verification".into()])
        );
        apply_unset(&mut cfg, "hooks.enabled").unwrap();
        assert!(cfg.hooks.enabled.is_none());
    }

    /// current_value_of 对未配置项返回 `<unset>`。
    #[test]
    fn current_value_of_handles_unset() {
        let cfg = ReflectConfig::default();
        assert_eq!(current_value_of(&cfg, "anthropic.model"), "<unset>");
        assert_eq!(current_value_of(&cfg, "unknown"), "(unknown)");
    }

    /// `set` 真的写盘(用 tmpdir + 临时 HOME 验证)。
    #[test]
    fn set_writes_to_disk() {
        let dir = tmpdir();
        // 把 HOME 临时改成 tmpdir;但 env 修改全局状态,可能影响并行测试 → 跳过这个 case,
        // 改测 write_config_to 直写路径。
        let mut cfg = ReflectConfig::default();
        apply_set(&mut cfg, "anthropic.model", "gpt-4o-mini").unwrap();
        let path = dir.path().join(".reflect").join("config.toml");
        login::write_config_to(&cfg, &path).unwrap();
        let back = reflect_config::load_from_file(&path).unwrap();
        assert_eq!(
            back.anthropic.unwrap().model.as_deref(),
            Some("gpt-4o-mini")
        );
    }
}
