//! 把协议顶层类型的极简 JSON Schema 导出到 stdout。
//!
//! M1.2 (协议桥) 用法 —— 前端运行 `npx json2ts` 把输出转为
//! `crates/reflect-gui/src/types/protocol.ts`,作为 IPC 载荷的 TS 类型来源。
//!
//! 实施说明:
//!
//! - 仅生成 `EventMsg` 与 `Submission` 的**顶层** schema(union discriminator
//!   `type`)。nested 结构不在 schema 中展开 —— 由前端的 `json2ts --unknownAny = false`
//!   配合 `ReflectEvent` 类型的`unknown`字段处理,见 `crates/reflect-gui/src/types/protocol.ts`。
//! - schemars derive 整个 30+ 子类型工作量超出 M1.2 范围。 M2.x 再追加 derive
//!   并重写本 example 为完整 schema 版本。
//! - 本文件不依赖 `schemars` derive, 直接 hand-write JSON。
//!
//! ## M1.2 用法
//!
//! ```bash
//! cargo run -p reflect-protocol --example dump_schema > /tmp/reflect-schema.json
//! cd crates/reflect-gui && npx json2ts /tmp/reflect-schema.json -o src/types/protocol.ts
//! ```

use serde_json::{Value, json};

/// 当前 `EventMsg` 已知 variant 列表。 与 `crates/reflect-protocol/src/event_msg.rs` 同步。
/// 新增 variant 时双写(此处 + proto 定义);M2.x 升级到 schemars derive 后去除。
const EVENT_MSG_VARIANTS: &[&str] = &[
    "session_configured",
    "turn_started",
    "turn_complete",
    "turn_aborted",
    "turn_rewound",
    "shutdown_complete",
    "agent_message",
    "agent_message_delta",
    "thinking_delta",
    "token_count",
    "tool_call_begin",
    "tool_call_end",
    "approval_request",
    "ask_user_question",
    "ask_user_input",
    "permission_bubble",
    "context_compacted",
    "error",
    "stream_error",
    "routing",
    "config_reloaded",
    "collab_started",
    "collab_message",
    "collab_finished",
    "mcp_server_started",
    "mcp_server_failed",
    "mcp_tool_invoked",
    "lsp_server_started",
    "lsp_server_failed",
    "plan_request",
    "plan_ready",
    "plan_approved",
    "plan_rejected",
    "permission_mode_changed",
];

/// 当前 `Op` 已知 variant 列表。 与 `crates/reflect-protocol/src/op.rs` 同步。
const OP_VARIANTS: &[&str] = &[
    "user_input",
    "compact",
    "interrupt",
    "rewind",
    "shutdown",
    "tool_approval",
    "hook_approval",
    "enter_plan_mode",
    "exit_plan_mode",
    "plan_approval",
    "set_effort",
    "ask_user_question_response",
    "ask_user_input_response",
    "set_permission_mode",
    "cycle_permission_mode",
];

fn main() {
    // Submission 提交结构 —— { id, op: { type, ... }, client_user_message_id?, trace? }(字段名为契约)
    // EventMsg ---- { id: str, msg: { type, ... } } (外层 Event 在 src-tauri 自行定义)
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "ReflectProtocol",
        "definitions": {
            "Submission": {
                "type": "object",
                "required": ["id", "op"],
                "properties": {
                    "id": { "type": "string", "format": "uuid" },
                    "op": {
                        "oneOf": OP_VARIANTS.iter().map(|v| json!({
                            "type": "object",
                            "required": ["type"],
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": [*v],
                                    "const": null,
                                }
                            }
                        })).collect::<Vec<_>>()
                    },
                    "client_user_message_id": { "type": ["string", "null"] },
                    "trace": { "type": ["object", "null"] }
                }
            },
            "EventMsg": {
                "type": "object",
                "required": ["type"],
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": EVENT_MSG_VARIANTS.iter().map(|s| Value::String((*s).into())).collect::<Vec<_>>()
                    }
                }
            }
        }
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("serialize schema")
    );
}
