//! 模块测试。从原文件内联 `#[cfg(test)] mod tests` 迁移而来。

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn tmp_workspace() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "reflect_ast_tool_test_{}_{}",
        std::process::id(),
        n
    ));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_ctx(workspace: &std::path::Path) -> reflect_tools::ToolContext {
    reflect_tools::ToolContext::for_workspace(workspace)
}

#[test]
fn tool_name_and_description_static() {
    let tool = AstTool::new();
    assert_eq!(tool.name(), "ast");
    // description 是简短的 1-2 句;action 名应出现;prefix-mode 文档已移到
    // `pattern` param 描述里(避免 description 嵌入教程和 system prompt 抢指令遵循)。
    assert!(tool.description().contains("list_languages"));
    assert!(!tool.description().contains("kind:"));
    let schema = tool.parameters_schema();
    let pattern_desc = schema["properties"]["pattern"]["description"]
        .as_str()
        .expect("pattern param must have a description");
    assert!(
        pattern_desc.contains("kind:")
            && pattern_desc.contains("regex:")
            && pattern_desc.contains("text:"),
        "prefix-mode docs should live in pattern param description, got: {pattern_desc}"
    );
}

#[test]
fn p0_required_permission_is_auto() {
    let tool = AstTool::new();
    assert_eq!(tool.required_permission(), PermissionMode::Auto);
    assert!(!tool.is_concurrency_safe());
}

#[test]
fn parameters_schema_has_action_required() {
    let tool = AstTool::new();
    let schema = tool.parameters_schema();
    let required = schema["required"].as_array().expect("required array");
    let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"action"),
        "schema should require 'action', got {names:?}"
    );
    let enums = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("action enum array");
    let values: Vec<&str> = enums.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        values,
        vec!["list_languages", "search", "replace", "rename_symbol"]
    );
}

#[tokio::test]
async fn list_languages_returns_default_grammars() {
    let tool = AstTool::new();
    let out = tool
        .execute(
            reflect_tools::ToolContext::default(),
            json!({"action": "list_languages"}),
        )
        .await
        .expect("execute list_languages");
    assert!(!out.is_error);
    match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => {
            assert!(text.contains("rust"), "missing rust: {text}");
            assert!(text.contains("python"), "missing python: {text}");
            assert!(text.contains("javascript"), "missing javascript: {text}");
        }
        other => panic!("expected Text block, got {other:?}"),
    }
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 6); // rust + ts + tsx + py + go + js
}

#[tokio::test]
async fn unknown_action_errors() {
    let tool = AstTool::new();
    let err = tool
        .execute(
            reflect_tools::ToolContext::default(),
            json!({"action": "frobnicate"}),
        )
        .await
        .expect_err("unknown action should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn search_kind_finds_function_items() {
    let ws = tmp_workspace();
    let src = "fn alpha() {}\nfn beta() {}\nstruct S;\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.rs",
                "pattern": "kind:function_item",
            }),
        )
        .await
        .expect("execute search kind:");
    assert!(!out.is_error, "got: {out:?}");
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 2);
    match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => {
            assert!(text.contains("alpha"), "missing alpha: {text}");
            assert!(text.contains("beta"), "missing beta: {text}");
            assert!(text.contains("\"file\": \"a.rs\""));
        }
        other => panic!("expected Text block, got {other:?}"),
    }
}

#[tokio::test]
async fn search_kind_unknown_returns_empty() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.rs",
                "pattern": "kind:nonexistent_kind_xyz",
            }),
        )
        .await
        .expect("execute search kind: unknown");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 0);
}

#[tokio::test]
async fn search_kind_rejects_missing_path() {
    let ws = tmp_workspace();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({"action": "search", "pattern": "kind:function_item"}),
        )
        .await
        .expect_err("missing path should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn search_kind_rejects_bad_pattern() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({"action": "search", "path": "a.rs", "pattern": "function_item"}),
        )
        .await
        .expect_err("missing prefix should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn search_kind_rejects_unsupported_extension() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.xyz"), "fn main() {}").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.xyz",
                "pattern": "kind:function_item",
            }),
        )
        .await
        .expect_err("unsupported extension should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn search_kind_python_function_definition() {
    let ws = tmp_workspace();
    std::fs::write(
        ws.join("a.py"),
        "def alpha():\n    pass\ndef beta():\n    pass\n",
    )
    .unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.py",
                "pattern": "kind:function_definition",
            }),
        )
        .await
        .expect("execute search kind: function_definition");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 2);
}

#[tokio::test]
async fn search_kind_tsx_finds_jsx_element() {
    let ws = tmp_workspace();
    std::fs::write(
        ws.join("a.tsx"),
        "const x = <div>hi</div>;\nfunction f() { return <span/>; }\n",
    )
    .unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.tsx",
                "pattern": "kind:jsx_element",
            }),
        )
        .await
        .expect("execute search kind: jsx_element");
    assert!(!out.is_error);
    assert!(out.metadata["count"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn search_regex_matches_fn_token() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn foo() {}\nfn bar() {}\nstruct S;\n").unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.rs",
                "pattern": "regex:^fn",
            }),
        )
        .await
        .expect("execute search regex:");
    assert!(!out.is_error);
    // 至少 2 个 `fn ` token 命中
    assert!(out.metadata["count"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn search_text_walks_workspace() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "alpha\nTODO: do thing\nbeta").unwrap();
    std::fs::write(ws.join("b.txt"), "TODO: also here").unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": ".",
                "pattern": "text:TODO",
            }),
        )
        .await
        .expect("execute search text:");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 2);
}

#[tokio::test]
async fn search_text_default_root_is_workspace() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "TODO: visible").unwrap();
    let tool = AstTool::new();
    // path 缺省 → 走 workspace 根
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({"action": "search", "pattern": "text:TODO"}),
        )
        .await
        .expect("execute search text: default root");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 1);
}

#[tokio::test]
async fn search_text_respects_gitignore() {
    let ws = tmp_workspace();
    std::fs::create_dir_all(ws.join("target")).unwrap();
    std::fs::write(ws.join("target/ignored.rs"), "TODO: hidden").unwrap();
    std::fs::write(ws.join("a.rs"), "TODO: visible").unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({"action": "search", "path": ".", "pattern": "text:TODO"}),
        )
        .await
        .expect("execute search text: gitignore");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 1);
    match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => {
            assert!(text.contains("a.rs"), "should include a.rs");
            assert!(
                !text.contains("target/ignored"),
                "should exclude target/: {text}"
            );
        }
        other => panic!("expected Text block, got {other:?}"),
    }
}

#[tokio::test]
async fn search_text_rejects_invalid_regex() {
    // text 模式 needle 不是 regex,不应被 regex 编译;这里仅 sanity check:
    // 包含 "[" 这种 regex 特殊字符也能字面命中。
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "let x = arr[0];\n").unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": ".",
                "pattern": "text:arr[0]",
            }),
        )
        .await
        .expect("text: with brackets");
    assert!(!out.is_error);
    assert_eq!(out.metadata["count"].as_u64().unwrap(), 1);
}

#[tokio::test]
async fn search_regex_compile_error_returns_invalid_args() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "search",
                "path": "a.rs",
                "pattern": "regex:[",
            }),
        )
        .await
        .expect_err("invalid regex should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn action_permission_routes_mutation_to_prompt() {
    let tool = AstTool::new();
    // read actions: Auto
    for action in ["list_languages", "search"] {
        let p = tool.action_permission(&json!({"action": action}));
        assert_eq!(
            p,
            PermissionMode::Auto,
            "{action} should be Auto, got {p:?}"
        );
    }
    // mutation actions: Prompt
    for action in ["replace", "rename_symbol"] {
        let p = tool.action_permission(&json!({"action": action}));
        assert_eq!(
            p,
            PermissionMode::Prompt,
            "{action} should be Prompt, got {p:?}"
        );
    }
    // missing action 走 default(Auto)
    assert_eq!(tool.action_permission(&json!({})), PermissionMode::Auto);
}

#[tokio::test]
async fn replace_action_writes_unified_diff() {
    let ws = tmp_workspace();
    let src = "fn foo() {}\nfn bar() {}\nstruct S;\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "replace",
                "path": "a.rs",
                "pattern": "kind:function_item",
                "replacement": "fn REPLACED() {}",
            }),
        )
        .await
        .expect("execute replace");
    assert!(!out.is_error, "got: {out:?}");
    // 文件被改写
    let new_src = std::fs::read_to_string(ws.join("a.rs")).unwrap();
    assert!(!new_src.contains("fn foo"));
    assert!(!new_src.contains("fn bar"));
    assert!(new_src.contains("fn REPLACED"));
    assert!(new_src.contains("struct S"));
    // ContentBlock::Diff 包含 unified diff
    assert!(matches!(
        &out.content[0],
        reflect_protocol::ContentBlock::Diff { .. }
    ));
    assert_eq!(out.metadata["target_kind"], "function_item");
}

#[tokio::test]
async fn replace_action_no_match_skips_write() {
    let ws = tmp_workspace();
    let src = "fn foo() {}\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "replace",
                "path": "a.rs",
                "pattern": "kind:nonexistent_kind_xyz",
                "replacement": "X",
            }),
        )
        .await
        .expect("execute replace no match");
    assert!(!out.is_error);
    assert_eq!(out.metadata["replacements"].as_u64().unwrap(), 0);
    // 文件保持原样
    assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), src);
}

#[tokio::test]
async fn replace_action_rejects_missing_replacement() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn foo() {}").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "replace",
                "path": "a.rs",
                "pattern": "kind:function_item",
            }),
        )
        .await
        .expect_err("missing replacement should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn replace_action_rejects_non_kind_pattern() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn foo() {}").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "replace",
                "path": "a.rs",
                "pattern": "regex:^fn",
                "replacement": "X",
            }),
        )
        .await
        .expect_err("non-kind pattern should error");
    match err {
        ToolError::InvalidArgs { message } => {
            assert!(message.contains("kind:"), "got: {message}");
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_symbol_writes_word_boundary_diff() {
    let ws = tmp_workspace();
    let src = "let foo = 1;\nlet foobar = 2;\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "rename_symbol",
                "path": "a.rs",
                "from": "foo",
                "to": "bar",
            }),
        )
        .await
        .expect("execute rename_symbol");
    assert!(!out.is_error, "got: {out:?}");
    let new_src = std::fs::read_to_string(ws.join("a.rs")).unwrap();
    // word-boundary:foobar 不动
    assert!(new_src.contains("let bar = 1"));
    assert!(new_src.contains("let foobar = 2"));
    assert_eq!(out.metadata["from"], "foo");
    assert_eq!(out.metadata["to"], "bar");
    assert_eq!(out.metadata["replacements"].as_u64().unwrap(), 1);
    // ContentBlock::Diff
    assert!(matches!(
        &out.content[0],
        reflect_protocol::ContentBlock::Diff { .. }
    ));
}

#[tokio::test]
async fn rename_symbol_no_match_skips_write() {
    let ws = tmp_workspace();
    let src = "let x = 1;\n";
    std::fs::write(ws.join("a.rs"), src).unwrap();
    let tool = AstTool::new();
    let out = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "rename_symbol",
                "path": "a.rs",
                "from": "missing_symbol",
                "to": "renamed",
            }),
        )
        .await
        .expect("execute rename no match");
    assert!(!out.is_error);
    assert_eq!(out.metadata["replacements"].as_u64().unwrap(), 0);
    assert_eq!(std::fs::read_to_string(ws.join("a.rs")).unwrap(), src);
}

#[tokio::test]
async fn rename_symbol_rejects_empty_from() {
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "let x = 1;").unwrap();
    let tool = AstTool::new();
    let err = tool
        .execute(
            make_ctx(&ws),
            json!({
                "action": "rename_symbol",
                "path": "a.rs",
                "from": "",
                "to": "renamed",
            }),
        )
        .await
        .expect_err("empty from should error");
    assert!(matches!(err, ToolError::InvalidArgs { .. }), "got: {err:?}");
}

#[tokio::test]
async fn mutation_actions_return_error_when_approval_denied() {
    use std::sync::Arc;
    let ws = tmp_workspace();
    std::fs::write(ws.join("a.rs"), "fn foo() {}").unwrap();
    let tool = AstTool::new();
    // 构造一个 ApprovalGate + 一个后台 task 监听 ApprovalRequest 立刻 Deny。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<reflect_protocol::Event>(4);
    let gate = Arc::new(reflect_tools::ApprovalGate::new(tx, "sub-test"));
    // 共享 gate 给 task —— 用 `with_session_allow` 把同一个 waiters 暴露出来。
    // 但更简单:clone gate 后 spawn,主路径用同一个 gate。
    let gate_for_task = gate.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let reflect_protocol::EventMsg::ApprovalRequest(req) = ev.msg {
                let _ = gate_for_task.complete(
                    &req.request_id,
                    reflect_protocol::ReviewDecision::Deny {
                        reason: "user said no".into(),
                    },
                );
                break;
            }
        }
    });

    let mut ctx = make_ctx(&ws);
    ctx.approval = Some(gate);
    let out = tool
        .execute(
            ctx,
            json!({
                "action": "replace",
                "path": "a.rs",
                "pattern": "kind:function_item",
                "replacement": "fn REPLACED() {}",
            }),
        )
        .await
        .expect("execute replace with denied approval");
    assert!(out.is_error, "should error on deny, got: {out:?}");
    match &out.content[0] {
        reflect_protocol::ContentBlock::Text { text } => {
            assert!(text.contains("approval denied"), "got: {text}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(out.metadata["approval"], "denied");
    // 文件未被改写
    let after = std::fs::read_to_string(ws.join("a.rs")).unwrap();
    assert!(after.contains("fn foo"));
    assert!(!after.contains("fn REPLACED"));
}
