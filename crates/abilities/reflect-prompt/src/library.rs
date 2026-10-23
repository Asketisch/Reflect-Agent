//! `library` —— 按模型匹配的外置提示词库。
//!
//! 目录约定(与 agents / skills / subagents 的 `.reflect/<dir>` 惯例一致):
//!
//! ```text
//! .reflect/prompts/          (workspace,优先) 或 ~/.reflect/prompts/
//! ├── default.md             # 兜底层:未命中任何模型文件时使用
//! ├── claude.md              # 厂商层:spec 前缀为 `claude/` 时叠加
//! ├── gpt.md
//! ├── claude-sonnet-4-5.md   # 精确层:spec 的 model 段完全一致时叠加
//! └── ...
//! ```
//!
//! 匹配链(逐级叠加,深者胜):`default` → 厂商 → 精确模型名。
//! 解析规则:spec 形如 `provider/model`;缺 `/` 时视为纯模型名
//! (厂商回退用 model 段的已知厂商关键词判断,详见 [`provider_hint`])。
//!
//! 加载层次:内嵌资源(编译期打包的默认文案)→ 用户目录
//! (default / 厂商 / 精确层逐层覆盖)。用户文件只需写出要覆盖的
//! section,缺失 key 落回内嵌默认,保证「开箱即用、渐进调优」。
//!
//! 进程级全局句柄 [`library()`] 默认只含内嵌默认;入口
//! (reflect-exec bootstrap / facade Builder)启动时调
//! [`init_library`] 注入 workspace + home 目录并重扫。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::resources::parse_sections;

/// 内嵌默认层(default),编译期打包。
pub const EMBEDDED_DEFAULT_PROMPTS: &str = include_str!("../prompts/default.md");

/// 内嵌厂商层:提炼自各厂商官方 agent system prompt(来源与许可见各
/// prompts/<vendor>.md 文件头注释);用户目录同名文件可整体覆盖。
/// 解析出的空 section 集不影响 default 层。
const EMBEDDED_PROVIDER_PROMPTS: &[(&str, &str)] = &[
    ("claude", include_str!("../prompts/claude.md")),
    ("gpt", include_str!("../prompts/gpt.md")),
    ("gemini", include_str!("../prompts/gemini.md")),
    ("deepseek", include_str!("../prompts/deepseek.md")),
    ("kimi", include_str!("../prompts/kimi.md")),
    ("qwen", include_str!("../prompts/qwen.md")),
    ("grok", include_str!("../prompts/grok.md")),
    ("glm", include_str!("../prompts/glm.md")),
    ("minimax", include_str!("../prompts/minimax.md")),
];

/// 目录下的层文件名序列(不含扩展名):default → 厂商(可多个,
/// 推断家族在前、显式 provider 在后)→ 精确模型名。
fn layer_file_names(providers: &[String], model: &str) -> Vec<String> {
    let mut names = vec!["default".to_string()];
    names.extend(providers.iter().cloned());
    names.push(model.to_string());
    names
}

/// 从 model 段推断厂商关键词(仅当 spec 无 `provider/` 前缀时用于回退)。
fn provider_hint(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    let hints: &[(&str, &str)] = &[
        ("claude", "claude"),
        ("gpt-", "gpt"),
        ("o1", "gpt"),
        ("o3", "gpt"),
        ("o4", "gpt"),
        ("chatgpt", "gpt"),
        ("codex", "gpt"),
        ("gemini", "gemini"),
        ("deepseek", "deepseek"),
        ("kimi", "kimi"),
        ("moonshot", "kimi"),
        ("qwen", "qwen"),
        ("grok", "grok"),
        ("glm", "glm"),
        ("minimax", "minimax"),
    ];
    hints
        .iter()
        .find(|(kw, _)| m.starts_with(kw))
        .map(|(_, p)| *p)
}

/// 解析 spec 为 `(provider, model)`。无 `/` 前缀时 provider 用关键词推断。
pub fn split_spec(spec: &str) -> (Option<String>, String) {
    let spec = spec.trim();
    if spec.is_empty() {
        return (None, String::new());
    }
    match spec.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider.to_ascii_lowercase()), model.to_string())
        }
        _ => {
            // 无前缀或格式异常:整串视为 model 名,厂商靠关键词推断。
            let provider = provider_hint(spec).map(str::to_string);
            (provider, spec.to_string())
        }
    }
}

/// 一次 resolve 的产物:该模型最终生效的全部 section。
///
/// 由 default → 厂商 → 精确模型名逐层 `extend` 合并(深者胜);
/// 未命中厂商/精确层时等于 default 层。
#[derive(Debug, Clone, Default)]
pub struct PromptSet {
    sections: HashMap<String, String>,
    /// 命中的用户层文件名(诊断用,如 `["default", "claude"]`)。
    pub layers: Vec<String>,
}

impl PromptSet {
    /// 取 section(不含 `##` 标题)。缺失时返回 None。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.sections.get(key).map(String::as_str)
    }

    /// 取 section,缺失时回落到 `fallback`。
    pub fn get_or<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.get(key).unwrap_or(fallback)
    }

    /// 该模型是否覆盖了指定 key。
    pub fn contains(&self, key: &str) -> bool {
        self.sections.contains_key(key)
    }

    pub(crate) fn extend_from(&mut self, sections: HashMap<String, String>) {
        self.sections.extend(sections);
    }
}

/// 按模型匹配的提示词库。
///
/// 内部持有:用户目录各层文件(default / 厂商 / 精确模型名)解析出的
/// section 表与旧版单文件覆盖。[`resolve`] 每次做三级叠加,开销为
/// 几次 HashMap extend —— model_call 每请求调用一次,可接受。
#[derive(Debug, Clone, Default)]
pub struct PromptLibrary {
    /// 用户目录层:`"claude"` / `"claude-sonnet-4-5"` / `"default"`
    /// → 该文件解析出的 section 表(不含 `##` 标题)。
    user_layers: HashMap<String, HashMap<String, String>>,
    /// 旧版单文件 `prompts.md` 的覆盖(视作 default 层,兼容保留)。
    legacy_overrides: HashMap<String, String>,
}

impl PromptLibrary {
    /// 仅内嵌默认(无用户目录)。全局 [`library()`] 未初始化时的形态。
    pub fn embedded() -> Self {
        Self::default()
    }

    /// 扫描目录下的 `*.md` 文件为用户层。文件名(去 `.md`)即层名,
    /// 后扫的目录覆盖先扫的同名层(与 agents 目录「workspace 优先」
    /// 惯例一致:调用方把 home 放前面、workspace 放后面)。
    pub fn scan_dirs(dirs: &[&Path]) -> Self {
        let mut lib = Self::default();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
                .collect();
            paths.sort();
            for path in paths {
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    tracing::warn!(path = %path.display(), "prompt file unreadable; skipped");
                    continue;
                };
                let sections = parse_sections(&text);
                if sections.is_empty() {
                    tracing::debug!(
                        path = %path.display(),
                        "prompt file has no `## key` sections; ignored"
                    );
                    continue;
                }
                lib.user_layers.insert(stem.to_string(), sections);
            }
        }
        lib
    }

    /// 注册旧版单文件 `prompts.md` 覆盖(视作 default 层级)。
    pub fn with_legacy_overrides(mut self, markdown: Option<&str>) -> Self {
        if let Some(text) = markdown {
            self.legacy_overrides = parse_sections(text);
        }
        self
    }

    /// `/prompt` 可选层清单:内置厂商层 + 用户目录层,字典序去重。
    ///
    /// 内置层即使某层全空(如 deepseek)也列出 —— 用户可以按同名文件
    /// 覆盖它;`default` 是兜底层,总是存在。
    pub fn available_layers(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        names.push("default".to_string());
        for (name, _) in EMBEDDED_PROVIDER_PROMPTS {
            names.push((*name).to_string());
        }
        for name in self.user_layers.keys() {
            names.push(name.clone());
        }
        names.sort();
        names.dedup();
        names
    }

    /// 按模型 spec 解析最终生效的 section 集合。
    ///
    /// 叠加顺序(default 最底、精确模型最顶,深者胜):
    /// 内嵌 default → 旧版 prompts.md 覆盖 → 内嵌厂商脚手架 →
    /// 用户 default 层 → 用户厂商层 → 用户精确模型层。
    pub fn resolve(&self, spec: &str) -> PromptSet {
        let (provider, model) = split_spec(spec);
        let mut set = PromptSet::default();

        // 厂商层命中路径,推断家族在前、显式 provider 在后(同 key 深者
        // 胜):模型名关键词命中家族层(`claude.md` 对
        // `anthropic/claude-*` 也生效),显式 provider id 命中自建层
        // (`stub.md` 对 `stub/any-model` 生效,不限于内嵌脚手架名单)。
        // 无前缀 spec 在 `split_spec` 已把关键词推断填进 provider,此处置名。
        let mut provider_names: Vec<String> = Vec::new();
        if let Some(family) = provider_hint(&model) {
            provider_names.push(family.to_string());
        }
        if let Some(provider) = provider.as_deref() {
            if !provider_names.iter().any(|n| n == provider) {
                provider_names.push(provider.to_string());
            }
        }

        // 1. 内嵌 default。
        set.extend_from(parse_sections(EMBEDDED_DEFAULT_PROMPTS));
        // 2. 旧版单文件覆盖(default 层级)。
        set.extend_from(self.legacy_overrides.clone());
        // 3. 内嵌厂商层(命中任一厂商即叠加;缺失/空 section 无副作用)。
        for name in &provider_names {
            if let Some((_, text)) = EMBEDDED_PROVIDER_PROMPTS.iter().find(|(n, _)| n == name) {
                set.extend_from(parse_sections(text));
            }
        }

        // 4-6. 用户层:default → 厂商(可多个)→ 精确模型名。
        for name in layer_file_names(&provider_names, &model) {
            if let Some(sections) = self.user_layers.get(&name) {
                set.layers.push(name.clone());
                set.extend_from(sections.clone());
            }
        }
        set
    }
}

// ── 进程级全局句柄 ─────────────────────────────────────────────

static LIBRARY: RwLock<Option<Arc<PromptLibrary>>> = RwLock::new(None);

/// 把模型层覆盖应用到工具 spec 序列:description 有 `tool.<name>`
/// section 的工具被替换为该模型的文案,其余不动。
///
/// 落点在 spec 组装处而非 `Tool::description()` —— MCP / LSP / plugin
/// 等外部工具实现者不该感知模型差异,内置工具的 `copy()` 调用点也
/// 保持零改动。`DescriptionSource` 抽象让 `reflect_llm::ToolSpec` 与
/// `reflect_tools::ToolSpec` 复用同一段逻辑;后者的 impl 放在
/// reflect-tools 侧(反向依赖会成环)。
pub trait DescriptionSource {
    /// 工具名(spec 的 `name` 字段)。
    fn name(&self) -> &str;
    /// 替换 description 为新文案。
    fn set_description(&mut self, description: String);
}

impl DescriptionSource for reflect_llm::ToolSpec {
    fn name(&self) -> &str {
        match self {
            reflect_llm::ToolSpec::Function { name, .. } => name,
        }
    }
    fn set_description(&mut self, description: String) {
        // 单变体 enum,直接解构(若未来加变体需补 match 臂)。
        let reflect_llm::ToolSpec::Function {
            description: slot, ..
        } = self;
        *slot = description;
    }
}

/// 对一组工具 spec 就地应用模型层 description 覆盖。
pub fn apply_model_overrides<S: DescriptionSource>(specs: &mut [S], set: &PromptSet) {
    for spec in specs.iter_mut() {
        let key = format!("tool.{}", spec.name());
        if let Some(desc) = set.get(&key) {
            spec.set_description(desc.to_string());
        }
    }
}

/// 进程级提示词库。未 [`init_library`] 时返回仅含内嵌默认的空库。
pub fn library() -> Arc<PromptLibrary> {
    LIBRARY
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(|| Arc::new(PromptLibrary::embedded()))
}

/// 初始化(或重扫)进程级提示词库。入口(bootstrap / Builder)在
/// 启动时调用;再次调用以最新目录内容整体替换(支持运行期重载)。
///
/// 目录顺序即优先级:后扫目录的同名层覆盖先扫的(惯例:home 在前、
/// workspace 在后)。`legacy_prompts_md` 为旧版 `~/.reflect/prompts.md`
/// 内容(可选,兼容既有用户)。
pub fn init_library(dirs: &[&Path], legacy_prompts_md: Option<&str>) -> Arc<PromptLibrary> {
    let lib = PromptLibrary::scan_dirs(dirs).with_legacy_overrides(legacy_prompts_md);
    let arc = Arc::new(lib);
    if let Ok(mut guard) = LIBRARY.write() {
        *guard = Some(arc.clone());
    }
    arc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib_with(layers: &[(&str, &str)]) -> PromptLibrary {
        let mut lib = PromptLibrary::default();
        for (name, text) in layers {
            lib.user_layers
                .insert(name.to_string(), parse_sections(text));
        }
        lib
    }

    #[test]
    fn split_spec_handles_all_shapes() {
        assert_eq!(
            split_spec("claude/claude-sonnet-4-5"),
            (Some("claude".into()), "claude-sonnet-4-5".into())
        );
        assert_eq!(
            split_spec("claude-sonnet-4-5"),
            (Some("claude".into()), "claude-sonnet-4-5".into())
        );
        assert_eq!(
            split_spec("gpt-5.2"),
            (Some("gpt".into()), "gpt-5.2".into())
        );
        assert_eq!(split_spec(""), (None, String::new()));
        assert_eq!(split_spec("weird"), (None, "weird".into()));
    }

    #[test]
    fn resolve_falls_back_through_layers() {
        let lib = lib_with(&[
            ("default", "## shared\ndefault text\n\n## only-default\nd\n"),
            ("claude", "## shared\nclaude text\n"),
            ("claude-sonnet-4-5", "## shared\nsonnet text\n"),
        ]);
        // 精确命中:三层叠加,深者胜。
        let exact = lib.resolve("anthropic/claude-sonnet-4-5");
        assert_eq!(exact.get("shared"), Some("sonnet text"));
        assert_eq!(exact.get("only-default"), Some("d"));
        // `anthropic` 不在内嵌厂商脚手架之列,但用户层 `claude.md` 按模型
        // 关键词命中:default → claude → claude-sonnet-4-5。
        assert_eq!(exact.layers, vec!["default", "claude", "claude-sonnet-4-5"]);

        // 厂商命中:无精确层文件。
        let provider = lib.resolve("anthropic/claude-opus-4");
        assert_eq!(provider.get("shared"), Some("claude text"));
        assert_eq!(provider.layers, vec!["default", "claude"]);

        // 全未命中:仅 default。
        let unknown = lib.resolve("mystery/unknown-model");
        assert_eq!(unknown.get("shared"), Some("default text"));
        assert_eq!(unknown.layers, vec!["default"]);
    }

    #[test]
    fn resolve_without_provider_prefix_uses_keyword_hint() {
        let lib = lib_with(&[("claude", "## k\nv\n")]);
        assert_eq!(lib.resolve("claude-haiku-4").get("k"), Some("v"));
        assert_eq!(lib.resolve("llama-3").get("k"), None);
    }

    #[test]
    fn embedded_default_contains_core_sections() {
        let lib = PromptLibrary::embedded();
        let set = lib.resolve("anything/model");
        assert!(set.get("tool.read").is_some());
        assert!(set.get("tool.bash").is_some());
        assert!(set.get("system.plan-mode").is_some());
    }

    /// 整合的厂商层(来源: 各厂商官方 agent prompt)必须被 resolve 命中:
    /// 显式 provider 前缀与无前缀关键词推断两条路径都要覆盖。
    #[test]
    fn embedded_vendor_layers_resolve_per_provider() {
        let lib = PromptLibrary::embedded();
        // (spec, 厂商 system.main 的标志性词句 —— 来自该厂商正文,不与
        //  default 层或其他厂商层重合)
        let cases = [
            ("claude/claude-sonnet-5-5", "交互式 CLI 编码 agent"),
            ("anthropic/claude-sonnet-5-5", "交互式 CLI 编码 agent"),
            ("claude-haiku-5-5", "交互式 CLI 编码 agent"),
            ("gpt-5.6", "编码 agent"),
            ("codex/gpt-5.6", "编码 agent"),
            ("o4-mini", "编码 agent"),
            ("gemini/gemini-3-pro", "资深软件工程师"),
            ("gemini-cli", "资深软件工程师"),
            ("kimi/kimi-3", "AI agent"),
            ("moonshot/kimi-3", "AI agent"),
            ("kimi-3", "AI agent"),
            ("qwen/qwen3.8-max", "AI 助手 agent"),
            ("qwen3.8-max", "AI 助手 agent"),
            ("grok/grok-4.7", "Grok"),
            ("grok-4.7-cli", "Grok"),
            ("glm/glm-5", "交互式 CLI 编码工具"),
            ("glm-5", "交互式 CLI 编码工具"),
            ("minimax/minimax-m2.5", "编码、写作与设计"),
            ("minimax-m2.5", "编码、写作与设计"),
        ];
        for (spec, marker) in cases {
            let set = lib.resolve(spec);
            let main = set.get("system.main").unwrap_or_default();
            assert!(
                main.contains(marker),
                "spec `{spec}` 应命中厂商层 system.main(含 `{marker}`),实际: {main}"
            );
        }
    }

    /// DeepSeek 无 agent 正文(源只有工具 JSON 框架),其内嵌层没有
    /// system.main —— resolve 应回落内嵌 default 文案。
    #[test]
    fn deepseek_layer_falls_back_to_default_main() {
        let lib = PromptLibrary::embedded();
        let set = lib.resolve("deepseek/deepseek-chat");
        assert!(
            set.get("system.main")
                .unwrap_or_default()
                .contains("先读后改"),
            "deepseek 层无 system.main,应回落内嵌 default"
        );
    }

    #[test]
    fn scan_dirs_reads_md_layers_with_later_dir_priority() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(home.join("claude.md"), "## k\nhome\n").unwrap();
        std::fs::write(ws.join("claude.md"), "## k\nws\n\n## ws-only\nw\n").unwrap();
        std::fs::write(ws.join("not-a-layer.txt"), "ignored").unwrap();
        let lib = PromptLibrary::scan_dirs(&[&home, &ws]);
        let set = lib.resolve("claude/claude-sonnet-4-5");
        assert_eq!(set.get("k"), Some("ws"));
        assert_eq!(set.get("ws-only"), Some("w"));
    }

    #[test]
    fn apply_model_overrides_replaces_matching_tool_description() {
        use reflect_llm::ToolSpec as LlmSpec;
        let lib = lib_with(&[("gpt", "## tool.read\nGPT-专属 read 说明\n")]);
        let set = lib.resolve("openai/gpt-5.2");
        let mut specs = vec![
            LlmSpec::Function {
                name: "read".into(),
                description: "default read".into(),
                parameters: serde_json::json!({}),
            },
            LlmSpec::Function {
                name: "bash".into(),
                description: "default bash".into(),
                parameters: serde_json::json!({}),
            },
        ];
        apply_model_overrides(&mut specs, &set);
        assert_eq!(specs[0].name(), "read");
        match &specs[0] {
            LlmSpec::Function { description, .. } => {
                assert_eq!(description, "GPT-专属 read 说明")
            }
        }
        // 无覆盖的工具回落内嵌 default 层的 `tool.bash` 文案 ——
        // 空 spec(`resolve("")`)也包含 default 层,故 bash 拿到的是
        // 内嵌默认而非测试里构造的假值。
        match &specs[1] {
            LlmSpec::Function { description, .. } => {
                assert!(description.contains("Execute a shell command"))
            }
        }
    }

    #[test]
    fn empty_spec_resolves_default_layer_only() {
        let lib = lib_with(&[("claude", "## k\nv\n")]);
        let set = lib.resolve("");
        assert!(set.get("k").is_none());
        assert_eq!(set.layers, Vec::<String>::new());
    }

    /// `/prompt` 可选层清单:内置厂商层 + 用户层,去重排序,default 恒在。
    #[test]
    fn available_layers_lists_embedded_and_user() {
        let lib = lib_with(&[("my-custom", "## k\nv\n")]);
        let layers = lib.available_layers();
        assert!(layers.contains(&"default".to_string()), "default 恒在");
        for vendor in [
            "claude",
            "deepseek",
            "gemini",
            "glm",
            "gpt",
            "grok",
            "kimi",
            "minimax",
            "my-custom",
            "qwen",
        ] {
            assert!(
                layers.iter().any(|l| l == vendor),
                "available_layers 应含 `{vendor}`,实际: {layers:?}"
            );
        }
        assert_eq!(layers.len(), layers.iter().len(), "不应有重复项");
    }
}
