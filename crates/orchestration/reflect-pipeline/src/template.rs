//! 极简字符串模板渲染 —— `{{input.X}}` / `{{nodes.X.outputs.Y}}` /
//! `{{topic}}` 占位符。
//!
//! 不引入 `minijinja` / `handlebars`,原因:
//! - 模板语法极简(单层变量查找 + 一层字段访问),无逻辑结构(if / for / filter)。
//! - `reflect-prompt` 用 `minijinja` 是 prompt 段落级别,语义不同。
//! - 引入反射依赖会让 `reflect-pipeline` 编译开销 + ~50KB,得不偿失。
//!
//! # 语法
//!
//! ```text
//! {{topic}}                          // pipeline 主题
//! {{input.<key>}}                    // runner 自己注入的 input map(key = 字符串)
//! {{nodes.<label>.outputs.<field>}}  // 上游节点 outputs 字段(整型 / 字符串 / 对象都支持)
//! ```
//!
//! # 错误
//!
//! - 未知占位符(`{{foo.bar}}` 但 `foo` 不存在)→ `Render("unknown placeholder: foo")`。
//! - 字段不存在(节点 `prd` 没有 `outputs.result`)→
//!   `Render("missing field 'outputs.result' on node 'prd'")`。
//!
//! 设计上**不**做"静默通过"(默认空串),防止拼错占位符导致 prompt 出现意外空白。

use std::collections::HashMap;

use serde_json::Value;

use crate::error::PipelineError;

/// 渲染模板,替换所有 `{{...}}` 占位符。
///
/// `inputs` = runner 自定义输入(如 plan 节点的 `topic`)。
/// `node_outputs` = 上游节点 outputs(label → outputs Value)。
pub fn render(
    template: &str,
    topic: &str,
    inputs: &HashMap<String, String>,
    node_outputs: &HashMap<String, Value>,
) -> Result<String, PipelineError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // 找到匹配的 `}}`
            if let Some(end_rel) = find_close(template, i + 2) {
                let path = template[i + 2..i + 2 + end_rel].trim();
                let resolved = resolve(path, topic, inputs, node_outputs)?;
                out.push_str(&resolved);
                i = i + 2 + end_rel + 2;
                continue;
            }
        }
        // 普通字符:按 UTF-8 字符边界追加,避免 `bytes[i]` 切割多字节字符。
        let ch_end = next_char_boundary(template, i);
        out.push_str(&template[i..ch_end]);
        i = ch_end;
    }
    Ok(out)
}

/// 在 `template[start..]` 中找第一个 `}}`,返回相对 start 的偏移。
/// 未找到返回 `None`。
fn find_close(template: &str, start: usize) -> Option<usize> {
    let bytes = template.as_bytes();
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i - start);
        }
        i = next_char_boundary(template, i);
    }
    None
}

/// 下一个 UTF-8 字符边界。
fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// 解析一个路径字符串(`topic` / `input.X` / `nodes.X.outputs.Y`)。
fn resolve(
    path: &str,
    topic: &str,
    inputs: &HashMap<String, String>,
    node_outputs: &HashMap<String, Value>,
) -> Result<String, PipelineError> {
    if path.is_empty() {
        return Err(PipelineError::Render("empty placeholder `{{}}`".into()));
    }
    let segments: Vec<&str> = path.split('.').collect();
    match segments[0] {
        "topic" => {
            // `{{topic}}` 走主题;`{{topic.foo}}` 不允许。
            if segments.len() != 1 {
                return Err(PipelineError::Render(format!(
                    "'topic' takes no subfield, got '{}'",
                    path
                )));
            }
            Ok(topic.to_string())
        }
        "input" => {
            // `{{input.<key>}}` 一层字段。
            if segments.len() != 2 {
                return Err(PipelineError::Render(format!(
                    "'input' requires exactly one key, got '{}'",
                    path
                )));
            }
            inputs
                .get(segments[1])
                .cloned()
                .ok_or_else(|| PipelineError::Render(format!("unknown input '{}'", segments[1])))
        }
        "nodes" => {
            // `{{nodes.<label>.outputs.<field>}}` 或 `{{nodes.<label>.outputs}}`
            // (整段 outputs 序列化为 JSON)。
            if segments.len() < 3 || segments[2] != "outputs" {
                return Err(PipelineError::Render(format!(
                    "expected 'nodes.<label>.outputs[.<field>]', got '{}'",
                    path
                )));
            }
            let label = segments[1];
            let outputs = node_outputs
                .get(label)
                .ok_or_else(|| PipelineError::Render(format!("unknown upstream node '{label}'")))?;
            if segments.len() == 3 {
                // 整段 outputs 序列化为 JSON 字符串。
                Ok(serde_json::to_string_pretty(outputs)
                    .map_err(|e| PipelineError::Render(e.to_string()))?)
            } else if segments.len() == 4 {
                let field = segments[3];
                value_to_string(outputs, field)
            } else {
                Err(PipelineError::Render(format!(
                    "deep path '{}' not supported (max 'nodes.<label>.outputs.<field>')",
                    path
                )))
            }
        }
        other => Err(PipelineError::Render(format!(
            "unknown placeholder root '{other}' (expected topic / input / nodes)"
        ))),
    }
}

/// 在 `outputs` Value 上找 `field`,转换为字符串。
/// - `Value::String(s)` → 原样。
/// - `Value::Null` / 不存在 → 报错(不允许静默通过)。
/// - 其他 JSON 值 → `to_string()` 序列化。
fn value_to_string(outputs: &Value, field: &str) -> Result<String, PipelineError> {
    let v = outputs.get(field).ok_or_else(|| {
        PipelineError::Render(format!(
            "missing field '{field}' on outputs (available: {:?})",
            outputs
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
                .unwrap_or_default()
        ))
    })?;
    match v {
        Value::Null => Err(PipelineError::Render(format!("field '{field}' is null"))),
        Value::String(s) => Ok(s.clone()),
        other => Ok(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inputs() -> HashMap<String, String> {
        HashMap::new()
    }

    fn empty_outputs() -> HashMap<String, Value> {
        HashMap::new()
    }

    #[test]
    fn no_placeholders_returns_verbatim() {
        let out = render("hello world", "topic-x", &empty_inputs(), &empty_outputs()).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn renders_topic_placeholder() {
        let out = render(
            "Plan for: {{topic}}",
            "build CLI",
            &empty_inputs(),
            &empty_outputs(),
        )
        .unwrap();
        assert_eq!(out, "Plan for: build CLI");
    }

    #[test]
    fn renders_input_placeholder() {
        let mut inputs = HashMap::new();
        inputs.insert("audience".into(), "engineers".into());
        let out = render(
            "Audience: {{input.audience}}",
            "x",
            &inputs,
            &empty_outputs(),
        )
        .unwrap();
        assert_eq!(out, "Audience: engineers");
    }

    #[test]
    fn renders_node_outputs_field() {
        let mut outs = HashMap::new();
        outs.insert(
            "plan".into(),
            serde_json::json!({ "result": "Use petgraph for 5 nodes" }),
        );
        let out = render(
            "Based on: {{nodes.plan.outputs.result}}",
            "x",
            &empty_inputs(),
            &outs,
        )
        .unwrap();
        assert_eq!(out, "Based on: Use petgraph for 5 nodes");
    }

    #[test]
    fn renders_node_outputs_whole_as_json() {
        let mut outs = HashMap::new();
        outs.insert(
            "plan".into(),
            serde_json::json!({ "result": "x", "metadata": { "a": 1 } }),
        );
        let out = render("Full: {{nodes.plan.outputs}}", "x", &empty_inputs(), &outs).unwrap();
        assert!(out.contains("\"result\""));
        assert!(out.contains("\"x\""));
        assert!(out.contains("\"a\""));
        assert!(out.contains("1"));
    }

    #[test]
    fn unknown_placeholder_root_errors() {
        let err = render("{{foo.bar}}", "t", &empty_inputs(), &empty_outputs()).unwrap_err();
        match err {
            PipelineError::Render(msg) => assert!(msg.contains("foo")),
            other => panic!("expected Render, got {other:?}"),
        }
    }

    #[test]
    fn missing_input_key_errors() {
        let err = render("{{input.ghost}}", "t", &empty_inputs(), &empty_outputs()).unwrap_err();
        match err {
            PipelineError::Render(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected Render, got {other:?}"),
        }
    }

    #[test]
    fn missing_node_outputs_field_errors() {
        let mut outs = HashMap::new();
        outs.insert("plan".into(), serde_json::json!({ "result": "x" }));
        let err = render("{{nodes.plan.outputs.ghost}}", "t", &empty_inputs(), &outs).unwrap_err();
        match err {
            PipelineError::Render(msg) => {
                assert!(msg.contains("ghost"));
                assert!(
                    msg.contains("result"),
                    "should list available fields: {msg}"
                );
            }
            other => panic!("expected Render, got {other:?}"),
        }
    }

    #[test]
    fn empty_placeholder_errors() {
        let err = render("{{}}", "t", &empty_inputs(), &empty_outputs()).unwrap_err();
        assert!(matches!(err, PipelineError::Render(_)));
    }

    #[test]
    fn multiple_placeholders_in_one_template() {
        let mut outs = HashMap::new();
        outs.insert("plan".into(), serde_json::json!({ "result": "use A" }));
        let out = render(
            "Topic={{topic}} Plan={{nodes.plan.outputs.result}}",
            "build X",
            &empty_inputs(),
            &outs,
        )
        .unwrap();
        assert_eq!(out, "Topic=build X Plan=use A");
    }

    #[test]
    fn non_ascii_template_preserves_utf8() {
        // 模板含中文,UTF-8 多字节字符不应被 `next_char_boundary` 切碎。
        let out = render(
            "主题:{{topic}}",
            "构建 CLI",
            &empty_inputs(),
            &empty_outputs(),
        )
        .unwrap();
        assert_eq!(out, "主题:构建 CLI");
    }
}
