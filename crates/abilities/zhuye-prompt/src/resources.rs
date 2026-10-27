//! 轻量 Markdown 文案资源(default 层)。
//!
//! 默认值内嵌在二进制中。启动后首次访问时会从 `ZHUYE_HOME` 或
//! `$HOME/.zhuye` 加载可选的旧版 `prompts.md`,且只替换该文件中
//! 出现的 section。所得 map 在进程余下生命周期内不可变。
//!
//! 按模型匹配的能力已迁至 [`crate::library`];本模块保留 `copy()`
//! 语义(default 层)供内置工具 description 使用,调用点零改动。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::library::EMBEDDED_DEFAULT_PROMPTS;

static PROMPTS: OnceLock<PromptResources> = OnceLock::new();

/// 内嵌默认值 + 启动时用户覆盖的组合。
#[derive(Debug, Clone)]
pub struct PromptResources {
    sections: HashMap<String, String>,
}

impl PromptResources {
    /// 从内嵌 Markdown 与可选覆盖文件构造资源。
    pub fn from_markdown(defaults: &str, overrides: Option<&str>) -> Self {
        let mut sections = parse_sections(defaults);
        if let Some(overrides) = overrides {
            sections.extend(parse_sections(overrides));
        }
        Self { sections }
    }

    /// 返回指定名称的 section(不含其 `##` 标题)。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.sections.get(key).map(String::as_str)
    }

    #[allow(dead_code)]
    fn load_from_home() -> Self {
        let override_path = zhuye_home().map(|home| home.join("prompts.md"));
        Self::load_from_path(override_path.as_deref())
    }

    #[allow(dead_code)]
    fn load_from_path(path: Option<&Path>) -> Self {
        let overrides = path.and_then(|path| std::fs::read_to_string(path).ok());
        Self::from_markdown(EMBEDDED_DEFAULT_PROMPTS, overrides.as_deref())
    }
}

/// 进程级 prompt 资源。文件系统至多读取一次。
///
/// 语义已并入模型层:`~/.zhuye/prompts.md` 的覆盖经 [`init_library`]
/// 的 `legacy_prompts_md` 参数注册(入口调用方负责传入);这里返回的
/// map 只保证 default 层级(不含任何模型层)。新代码应使用
/// [`crate::library::resolve`]。
///
/// **注意**:本函数初始化**绝不能**触碰进程级 LIBRARY —— 它在首次
/// `copy()`(工具注册 / spec 组装)时惰性执行,晚于入口的
/// `init_library(&[dirs..])`;若在此重置库会把已扫描的用户模型层整体
/// 丢弃(实测 Bug:工具调用回环后 system.main 回退内嵌默认)。
pub fn prompts() -> &'static PromptResources {
    PROMPTS.get_or_init(|| {
        PromptResources::load_from_path(zhuye_home().map(|home| home.join("prompts.md")).as_deref())
    })
}

/// 返回必需的文案 section;仅在内嵌资源损坏或调用方请求未知 key 时
/// 才 panic。
pub fn copy(key: &str) -> &'static str {
    prompts()
        .get(key)
        .unwrap_or_else(|| panic!("missing prompt copy section: {key}"))
}

fn zhuye_home() -> Option<PathBuf> {
    std::env::var_os("ZHUYE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".zhuye")))
}

/// 解析 `## key` 分节的 Markdown 为有序表。库内多个模块共用。
pub(crate) fn parse_sections(markdown: &str) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut current_key: Option<&str> = None;
    let mut current_body = String::new();

    let finish = |key: Option<&str>, body: &mut String, sections: &mut HashMap<String, String>| {
        if let Some(key) = key {
            let body = body.trim().to_string();
            if !key.is_empty() && !body.is_empty() {
                sections.insert(key.to_string(), body);
            }
        }
        body.clear();
    };

    for line in markdown.lines() {
        if let Some(key) = line.strip_prefix("## ") {
            finish(current_key, &mut current_body, &mut sections);
            current_key = Some(key.trim());
        } else if current_key.is_some() {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        }
    }
    finish(current_key, &mut current_body, &mut sections);
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_resource_contains_system_and_all_builtin_tool_sections() {
        let resources = PromptResources::from_markdown(EMBEDDED_DEFAULT_PROMPTS, None);
        assert!(resources.get("system.plan-mode").is_some());
        let tool_count = resources
            .sections
            .keys()
            .filter(|key| key.starts_with("tool."))
            .count();
        assert_eq!(tool_count, 23);
    }

    #[test]
    fn override_replaces_only_present_sections() {
        let resources = PromptResources::from_markdown(
            "# Defaults\n\n## system.a\ndefault a\n\n## tool.b\ndefault b\n",
            Some("# User overrides\n\n## tool.b\ncustom b\n"),
        );
        assert_eq!(resources.get("system.a"), Some("default a"));
        assert_eq!(resources.get("tool.b"), Some("custom b"));
    }

    #[test]
    fn missing_or_unreadable_override_keeps_embedded_defaults() {
        let resources = PromptResources::load_from_path(Some(Path::new(
            "/path/that/cannot/exist/zhuye-prompts.md",
        )));
        assert!(
            resources
                .get("system.plan-mode")
                .is_some_and(|text| text.contains("ExitPlanMode"))
        );
    }

    #[test]
    fn parser_ignores_document_title_and_empty_sections() {
        let sections = parse_sections("# Title\nintro\n## empty\n\n## kept\n value \n");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections.get("kept").map(String::as_str), Some("value"));
    }
}
