//! `template` —— 基于 minijinja 的 `{{ var }}` 变量替换封装。
//!
//! Reflect 的 prompt manager 使用 Jinja2 替换;本实现采用 minijinja
//! (原生支持 Jinja2 语法)。v0 只做变量替换 —— 不支持 `{% if %}` /
//! `{% for %}` 块(与 Reflect `manager.get_prompt` 行为一致)。

use minijinja::{Environment, Value};
use thiserror::Error;

/// 渲染 prompt 模板时可能出现的错误。
#[derive(Debug, Error)]
pub enum PromptError {
    /// minijinja 报告了语法 / 求值错误。
    #[error("template render error: {0}")]
    Render(String),
}

/// 用给定 JSON 对象渲染 `template`。只支持 `{{ var }}` 变量替换;
/// 非严格模式下缺失变量渲染为空字符串(与 Reflect 对
/// `{{ task_content }}` 类占位符空值的宽容行为一致)。
pub fn render(template: &str, vars: &serde_json::Value) -> Result<String, PromptError> {
    let value: Value = serde_json::from_str(&serde_json::to_string(vars).unwrap_or_default())
        .unwrap_or(Value::UNDEFINED);
    let mut env = Environment::new();
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    // 未定义变量默认行为:缺失变量渲染为 "",让 Reflect 风格的
    // "Workspace: {{ workspace }}" 模板在未提供 workspace 时仍渲染干净。
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Lenient);
    let tmpl = env
        .template_from_str(template)
        .map_err(|e| PromptError::Render(e.to_string()))?;
    tmpl.render(value)
        .map_err(|e| PromptError::Render(e.to_string()))
}

/// 与 [`render`] 相同,但接受显式上下文 —— 便于调用方持有
/// `serde_json::Map` 而非 `Value` 的场景。
pub fn render_with<S: serde::Serialize>(template: &str, vars: S) -> Result<String, PromptError> {
    let value = serde_json::to_value(vars).unwrap_or(serde_json::Value::Null);
    render(template, &value)
}

/// 最常见场景的便捷封装:单一 `{{ task_content }}` 占位符。
/// 等价于 `render(template, &serde_json::json!{"task_content": ...})`。
pub fn render_task_content(template: &str, task_content: &str) -> Result<String, PromptError> {
    render(
        template,
        &serde_json::json!({ "task_content": task_content }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_simple_var() {
        let out = render("Hello {{ name }}", &serde_json::json!({"name": "world"})).unwrap();
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn render_handles_missing_var_lenient() {
        let out = render("Hello {{ name }}", &serde_json::json!({})).unwrap();
        assert_eq!(out, "Hello ");
    }

    #[test]
    fn render_with_object() {
        let out = render_with("a={{a}}, b={{b}}", serde_json::json!({"a": 1, "b": 2})).unwrap();
        assert_eq!(out, "a=1, b=2");
    }

    #[test]
    fn render_task_content_works() {
        let out = render_task_content("Task: {{ task_content }}", "do the thing").unwrap();
        assert_eq!(out, "Task: do the thing");
    }

    #[test]
    fn render_plain_text_passes_through() {
        let out = render("no vars here", &serde_json::json!({})).unwrap();
        assert_eq!(out, "no vars here");
    }

    #[test]
    fn render_trims_around_tags() {
        // trim_blocks + lstrip_blocks:`{% if %}` 是 tag block,
        // lstrip 去掉块前空白,trim 去掉块后的换行。
        let out = render(
            "header\n  {% if true %}X{% endif %}\nfooter",
            &serde_json::json!({}),
        )
        .unwrap();
        // 裁剪后:"header\n" + "X" + "footer" → "header\nXfooter"
        assert_eq!(out, "header\nXfooter");
    }

    #[test]
    fn render_error_on_invalid_template() {
        // 未闭合大括号 = 语法错误。
        let err = render("{{ unclosed", &serde_json::json!({})).unwrap_err();
        match err {
            PromptError::Render(_) => {}
        }
    }
}
