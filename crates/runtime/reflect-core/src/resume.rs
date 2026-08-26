//! resume 历史 —— `RolloutRecord` → `ChatMessage` 的纯映射。
//!
//! 从 `reflect-exec::bootstrap_resume` 抽取(exec 是 CLI 入口,GUI 等其它
//! 宿主需要同一映射却不应依赖 exec crate)。无 I/O:调用方自行 replay
//! JSONL 后把记录切片传进来。
//!
//! 语义(与原 bootstrap_resume 逐字一致,单一事实源在此):
//! - User 消息:支持 legacy `Value::String` 与 `Vec<ContentBlock>` 两种
//!   content 形态;仅 Text/Image 参与,user 侧的 ToolUse/ToolResult 忽略。
//! - Assistant 消息:Text → `AssistantContent.text`;ToolUse →
//!   `tool_calls`;ToolResult → 独立 `ChatMessage::Tool` 且**紧跟**
//!   Assistant 之后(provider 要求 assistant/tool 角色交替)。工具对
//!   忠实重建,不丢弃。
//! - `Compaction`:取最近一次非空 summary,前置为合成的 `System` 消息。
//!   注意:compaction 之前的旧消息**不丢弃**(全量历史 + 摘要前置),
//!   上下文收敛交给 Compactor 运行时处理。
//! - 其余记录(SessionMeta/Fork/Checkpoint/Rewind/TokenCount/Plan*/
//!   PermissionModeChanged/DiscussionTranscript)是审计轨迹,不注入对话。

use reflect_llm::ChatMessage;
use reflect_protocol::RolloutRecord;

/// 把 rollout 记录切片映射为适用于 `AgentThread` 的历史消息。
///
/// 空 / 全审计记录的输入 → 空输出。纯函数,可重入;`&[..]` 借用让调用方
/// 保留记录所有权(GUI 侧同一份 records 既喂本函数又序列化给前端水合)。
pub fn records_to_preload(records: &[RolloutRecord]) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut last_summary: Option<String> = None;

    for r in records {
        match r {
            RolloutRecord::Message {
                turn_id: _,
                role,
                content,
            } => {
                match role {
                    // ── User 消息 ───────────────────────────────────────────
                    // v1.2 P2:支持两种 content 形态。
                    //   Value::String —— 旧格式(裸文本,向后兼容)。
                    //   Value::Array  —— 新格式(protocol ContentBlock 数组,
                    //                    含 Text/Image,由 submission_loop 落盘)。
                    reflect_protocol::MessageRole::User => match content {
                        serde_json::Value::String(s) => {
                            messages.push(ChatMessage::User(reflect_llm::UserContent {
                                blocks: vec![reflect_llm::ContentBlock::Text { text: s.clone() }],
                            }));
                        }
                        arr @ serde_json::Value::Array(_) => {
                            if let Ok(blocks) =
                                serde_json::from_value::<Vec<reflect_protocol::ContentBlock>>(arr.clone())
                            {
                                // protocol → llm 层映射(仅 Text/Image 两变体,
                                // 与 user_input_items_to_messages 的输入域一致)。
                                let llm_blocks: Vec<_> = blocks
                                    .into_iter()
                                    .filter_map(|b| match b {
                                        reflect_protocol::ContentBlock::Text { text } => {
                                            Some(reflect_llm::ContentBlock::Text { text })
                                        }
                                        reflect_protocol::ContentBlock::Image { data, mime_type } => {
                                            Some(reflect_llm::ContentBlock::Image { data, mime_type })
                                        }
                                        // user 消息不含 ToolUse/ToolResult/Diff,
                                        // 出现则忽略(resume 不应注入结构化工具块)。
                                        _ => None,
                                    })
                                    .collect();
                                if !llm_blocks.is_empty() {
                                    messages.push(ChatMessage::User(reflect_llm::UserContent {
                                        blocks: llm_blocks,
                                    }));
                                }
                            }
                        }
                        _ => {}
                    },
                    // ── Assistant 消息 ──────────────────────────────────────
                    // v1.2 P2:支持两种 content 形态。
                    //   Value::String —— 旧格式(最后一条纯文本,向后兼容)。
                    //   Value::Array  —— 新格式(protocol ContentBlock 数组,
                    //                    Text + ToolUse + ToolResult,完整无损)。
                    //
                    // 新格式拆分逻辑:Text → AssistantContent.text;ToolUse →
                    // AssistantContent.tool_calls;ToolResult → 独立 ChatMessage::Tool
                    // (provider 要求 assistant/tool 角色交替,ToolResult 不能内嵌)。
                    // 顺序保证:写入侧 latest_content 为 Text→ToolUse→ToolResult,
                    // 映射时 Assistant(含 ToolUse)先 push、Tool 后 push。
                    reflect_protocol::MessageRole::Assistant => match content {
                        serde_json::Value::String(s) => {
                            messages.push(ChatMessage::Assistant(
                                reflect_llm::AssistantContent {
                                    text: Some(s.clone()),
                                    ..Default::default()
                                },
                            ));
                        }
                        arr @ serde_json::Value::Array(_) => {
                            if let Ok(blocks) =
                                serde_json::from_value::<Vec<reflect_protocol::ContentBlock>>(arr.clone())
                            {
                                let mut text_parts: Vec<String> = Vec::new();
                                let mut tool_calls: Vec<reflect_llm::ToolCallRequest> = Vec::new();
                                // ToolResult 先收集,循环结束后再 push —— 保证
                                // Assistant(含 ToolUse)在前、Tool(ToolResult)
                                // 在后的顺序(provider 要求 assistant→tool 交替)。
                                let mut tool_results: Vec<(String, reflect_protocol::ToolOutput)> =
                                    Vec::new();
                                for b in blocks {
                                    match b {
                                        reflect_protocol::ContentBlock::Text { text }
                                            if !text.is_empty() =>
                                        {
                                            text_parts.push(text);
                                        }
                                        reflect_protocol::ContentBlock::ToolUse { id, name, args } => {
                                            tool_calls.push(reflect_llm::ToolCallRequest {
                                                id,
                                                name,
                                                arguments: args,
                                            });
                                        }
                                        reflect_protocol::ContentBlock::ToolResult {
                                            call_id,
                                            output,
                                        } => {
                                            tool_results.push((call_id, output));
                                        }
                                        // Image / Diff 在 assistant 消息里不常见,
                                        // 出现则忽略(resume 不注入非文本 assistant 块)。
                                        _ => {}
                                    }
                                }
                                if !text_parts.is_empty() || !tool_calls.is_empty() {
                                    messages.push(ChatMessage::Assistant(
                                        reflect_llm::AssistantContent {
                                            text: if text_parts.is_empty() {
                                                None
                                            } else {
                                                Some(text_parts.join("\n"))
                                            },
                                            tool_calls,
                                            thinking: None,
                                        },
                                    ));
                                }
                                // ToolResults 在 Assistant 之后 push(provider
                                // 要求 assistant→tool 交替)。复用 from_output
                                // 完成 protocol→llm 层映射(保留 Image 等多模态)。
                                for (call_id, output) in tool_results {
                                    messages.push(ChatMessage::Tool(
                                        reflect_llm::ToolResult::from_output(call_id, &output),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            RolloutRecord::Compaction { summary, .. } => {
                if !summary.is_empty() {
                    last_summary = Some(summary.clone());
                }
            }
            RolloutRecord::SessionMeta { .. }
            | RolloutRecord::Fork { .. }
            | RolloutRecord::DiscussionTranscript { .. }
            // v1.2 P0-3:checkpoint / rewind 是工作区 marker,resume 历史
            // 时丢弃(只回放对话消息)。
            | RolloutRecord::Checkpoint { .. }
            | RolloutRecord::Rewind { .. }
            // v1.x:TokenCount 是 per-turn 统计快照(累计展示交给 CLI ls/show),
            // resume 历史时丢弃 —— 对话语义不受 token 计数影响。
            | RolloutRecord::TokenCount { .. }
            // v1.x Plan mode:PlanRequest / PlanReady / PlanRejected /
            // PermissionModeChanged 是 plan 生命周期与权限状态轨迹的审计
            // 记录。resume 回放时只重建 LLM 对话消息,plan 上下文不注入
            // (避免把已废弃的 plan_id / 旧权限态污染新会话)。
            | RolloutRecord::PlanRequest { .. }
            | RolloutRecord::PlanReady { .. }
            | RolloutRecord::PlanRejected { .. }
            | RolloutRecord::PermissionModeChanged { .. } => {}
        }
    }

    // 加一条合成的 System 消息,内容为最近一次 compaction 的摘要,
    // 让恢复后的 thread 拥有最新上下文。
    if let Some(s) = last_summary {
        messages.insert(0, ChatMessage::System(s));
    }
    messages
}
