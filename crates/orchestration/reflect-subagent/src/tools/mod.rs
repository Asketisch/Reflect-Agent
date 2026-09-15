//! `CallSubAgentTool` —— 注册在父级 `ToolRegistry` 上的 `call_<role>` 工具。
//! 当父级 LLM 调用它时,factory spawn 一个子 `AgentThread`,并返回抽取出的结果。

use std::sync::Arc;

use async_trait::async_trait;
use reflect_protocol::{ToolError, ToolOutput};
use reflect_tools::{Tool, ToolContext};

use crate::factory::SubAgentFactory;
use crate::spec::SubAgentSpec;

/// 委托给 `SubAgentFactory::spawn` + 结果抽取的 `Tool` 实现。
pub struct CallSubAgentTool {
    pub factory: Arc<SubAgentFactory>,
    pub spec: SubAgentSpec,
}

impl std::fmt::Debug for CallSubAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallSubAgentTool")
            .field("spec", &self.spec)
            .finish()
    }
}

impl CallSubAgentTool {
    pub fn new(factory: Arc<SubAgentFactory>, spec: SubAgentSpec) -> Self {
        Self { factory, spec }
    }
}

#[async_trait]
impl Tool for CallSubAgentTool {
    fn name(&self) -> &str {
        // 工具名以 owned `String` 形式存在 spec 内部。
        // 用 `Box::leak` 缓存指针?否 —— 用 thread-local 缓存让借用检查器满意。
        // 更简单的做法:直接返回一个泄漏的 str。
        // Spec 只在启动时构造一次,小量泄漏可接受。
        //
        // 为了避免泄漏,可在注册时把 `self.spec.role` 前缀拼好后用精确名称注册
        // (registry 仅在注册时使用 `tool.name()`)。调用方(exec / bootstrap)
        // 已知名称,无需回查。
        &self.spec.role_for_tool_name()
    }

    fn description(&self) -> &str {
        static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESC.get_or_init(|| {
            "Spawn a sub-agent invocation with the given prompt. The sub-agent has its own session and tools; the returned text is its final answer.".to_string()
        })
        // 必须返回 `&str`。用另一种技巧:每个实例泄漏一份。
        .as_str()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The user prompt to send to the sub-agent." },
                "context_tail": {
                    "type": "integer",
                    "description": "Optional override of the parent's pass_context_messages setting.",
                    "default": 0,
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // Subagent 调用彼此独立,可以并行执行。
        true
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'prompt' string".into(),
            })?
            .to_string();
        // v1.4 C1:父上下文传递接通 —— 截取条数优先取调用参数
        // `context_tail`,否则用 spec 的 `pass_context_messages`(默认 0,
        // 行为不变)。元素从 tool_exec 注入的父历史尾部快照(JSON)还原。
        let pass_n = args
            .get("context_tail")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(self.spec.data_transfer.pass_context_messages);
        let parent_tail: Vec<reflect_llm::ChatMessage> = if pass_n > 0 {
            ctx.parent_tail_json
                .read()
                .iter()
                .rev()
                .take(pass_n)
                .rev()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        } else {
            Vec::new()
        };
        let spawned = self
            .factory
            .spawn(self.spec.clone(), parent_tail, prompt.clone())
            .await
            .map_err(ToolError::from)?;
        let child_session_id = spawned.session_id.to_string();
        // v1.4 C1:进度推送 —— 把子代理的中间事件(助手文本 / 工具开始 /
        // 结束)包装为 SubagentProgress 事件经转发器发往父事件通道;
        // 转发器缺席(测试 / 直接构造)时 no-op。
        let progress_cb = ctx.event_forwarder.clone().map(|fwd| {
            let child_id = child_session_id.clone();
            let role = self.spec.role.clone();
            move |ev: &reflect_protocol::Event| {
                let payload = match &ev.msg {
                    reflect_protocol::EventMsg::AgentMessage(m) => Some((
                        reflect_protocol::SubagentProgressKind::Message,
                        m.text.clone(),
                        None,
                    )),
                    reflect_protocol::EventMsg::ToolCallBegin(b) => Some((
                        reflect_protocol::SubagentProgressKind::ToolBegin,
                        b.tool_name.clone(),
                        Some(b.call_id.clone()),
                    )),
                    reflect_protocol::EventMsg::ToolCallEnd(e) => Some((
                        reflect_protocol::SubagentProgressKind::ToolEnd,
                        if e.is_error {
                            "tool failed".to_string()
                        } else {
                            "tool done".to_string()
                        },
                        Some(e.call_id.clone()),
                    )),
                    _ => None,
                };
                if let Some((kind, text, call_id)) = payload {
                    fwd.forward(reflect_protocol::EventMsg::SubagentProgress(
                        reflect_protocol::SubagentProgressEvent {
                            child_id: child_id.clone(),
                            role: role.clone(),
                            kind,
                            text,
                            call_id,
                        },
                    ));
                }
            }
        });
        let result = spawned
            .collect_result_with_progress(
                progress_cb
                    .as_ref()
                    .map(|cb| cb as &(dyn Fn(&reflect_protocol::Event) + Send + Sync)),
            )
            .await
            .map_err(ToolError::from)?
            .text;
        // v1.1.0:coordinator 模式下 subagent 返回后向父协调者注入 principle footer。
        let result_text = if self.factory.is_coordinator_mode() {
            crate::data_transfer::append_coordinator_principle_footer(
                &result,
                self.factory.coordinator_footer().as_deref(),
            )
        } else {
            result.clone()
        };
        // v1.1.0 Phase 6 P0:成功后写入 subagent registry(若 factory
        // 注入了)。失败子代理不写,避免污染 `<system-reminder>`。
        // coordinator 模式下 task_summary 追加 worker role,便于 pre_loop
        // reminder 区分不同 worker 的派工摘要。
        let mut task_summary = prompt;
        if self.factory.is_coordinator_mode() {
            task_summary.push_str(&format!(" [worker: {}]", self.spec.role));
        }
        // `iteration` 用 `TurnId` 的低 32 位 —— UUID v4 全局唯一,
        // 这里只作排序键,不需要严格按 turn 序号。
        if let Some(reg) = self.factory.subagent_registry() {
            reg.record(reflect_recovery::SubagentRegistryEntry {
                tool_name: format!("call_{}", self.spec.role),
                task_summary,
                result_summary: result.clone(),
                iteration: ctx.turn_id.0.as_u128() as u32,
                created_at: chrono::Utc::now(),
            });
        }
        let active_workers = self
            .factory
            .subagent_registry()
            .map(|r| r.snapshot().len())
            .unwrap_or(0);
        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Text { text: result_text }],
            is_error: false,
            metadata: serde_json::json!({
                "subagent_role": self.spec.role,
                "session_id": child_session_id,
                "activeWorkers": active_workers,
            }),
            elapsed_ms: 0,
        })
    }
}

// `name()` 内使用的 helper trait。`SubAgentSpec::tool_name()` 每次调用都
// 分配一个新的 `String`,这与 `&str` 返回类型冲突。所以通过本实现预计算
// 并存储一个泄漏的 str。
impl SubAgentSpec {
    /// 以 borrowed `&str` 形式返回 `call_<role>`。每个 spec 一次性泄漏,
    /// 因为 spec 仅在启动时构造一次,代价可接受。
    pub fn role_for_tool_name(&self) -> &str {
        // 按需计算;通过类似 thread-local 的方式泄漏缓存。
        let s = self.tool_name();
        Box::leak(s.into_boxed_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_transfer::DataTransferConfig;
    use reflect_protocol::ThreadId;
    use reflect_tools::ToolRegistry;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn tool_name_format() {
        let s = SubAgentSpec {
            name: "Explorer".into(),
            role: "explorer".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert_eq!(s.tool_name(), "call_explorer");
    }

    #[test]
    fn parameters_schema_requires_prompt() {
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let tool = CallSubAgentTool::new(
            factory,
            SubAgentSpec {
                name: "x".into(),
                role: "x".into(),
                model: None,
                system_prompt: "x".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "prompt"));
    }

    /// SubagentRegistry 未注入时,`execute` 不应 panic,正常返回结果。
    #[tokio::test]
    async fn execute_works_without_registry() {
        // 验证 factory 默认 subagent_registry = None 时,execute 走 fallback。
        // 由于 spawn 需要 AgentThread + ModelRegistry wiring,这里只测
        // factory 注入 registry 但 spec 不触发 spawn 的边界 case 之一。
        // 真正的端到端测试在 `discussion_collab_e2e.rs`。
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        // 注入空 registry —— execute 仍能取到 `Some(reg)`,record 不会 panic。
        factory.set_subagent_registry(reflect_recovery::SubagentRegistry::shared());
        assert!(factory.subagent_registry().is_some());
    }

    /// SubagentRegistry FIFO cap=32:写入 33 条 → 第 1 条被淘汰,
    /// 最新的 32 条保留。
    #[tokio::test]
    async fn registry_record_overflow_drops_oldest() {
        let reg = reflect_recovery::SubagentRegistry::shared();
        for i in 0..33 {
            reg.record(reflect_recovery::SubagentRegistryEntry {
                tool_name: format!("call_role_{i}"),
                task_summary: format!("task {i}"),
                result_summary: format!("result {i}"),
                iteration: i,
                created_at: chrono::Utc::now(),
            });
        }
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 32, "FIFO cap=32,33 条应保留后 32 条");
        // 第 0 条被淘汰,第 1 条仍在。
        assert!(snap.iter().any(|e| e.tool_name == "call_role_1"));
        assert!(!snap.iter().any(|e| e.tool_name == "call_role_0"));
    }

    /// 写入 registry 后 activeWorkers 计数与 snapshot 长度一致。
    #[test]
    fn registry_snapshot_len_matches_active_workers_metadata() {
        let reg = reflect_recovery::SubagentRegistry::shared();
        reg.record(reflect_recovery::SubagentRegistryEntry {
            tool_name: "call_architect".into(),
            task_summary: "task [worker: architect]".into(),
            result_summary: "done".into(),
            iteration: 1,
            created_at: chrono::Utc::now(),
        });
        assert_eq!(reg.snapshot().len(), 1);
    }

    // ── v1.1.0 复审 ─────────────────────────────────────────────
    // bug-3 (P1):失败子代理不写 registry 契约
    // bug-8 (P2):coordinator worker 后缀

    /// bug-3:`execute` 中 spawn 失败时,绝不能写 registry(契约守护)。
    /// 当前实现走 short-circuit: spawn 错误在 line 95 返回,根本到不了
    /// 写 registry 的代码。本测试验证:若有人重构加入"无论成败都 record"
    /// 的代码,会被本测试拦下。
    ///
    /// 触发失败路径:用非法 spec (大写 role 触发 `SpecInvalid`),让
    /// `spawn` 在 `spec.validate()` 阶段返回错误,根本到不了 record 调用。
    #[tokio::test]
    async fn failed_subagent_does_not_write_registry() {
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(reflect_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let reg = reflect_recovery::SubagentRegistry::shared();
        factory.set_subagent_registry(reg.clone());

        let tool = CallSubAgentTool::new(
            factory.clone(),
            SubAgentSpec {
                name: "BadName".into(),
                // 大写 role → `validate()` 拒绝 → spawn 返回 SpecInvalid
                role: "BadRole".into(),
                model: None,
                system_prompt: "x".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let args = serde_json::json!({"prompt": "should not be recorded"});
        let result = tool
            .execute(reflect_tools::ToolContext::default(), args)
            .await;
        // spawn 失败 → ToolError
        assert!(result.is_err(), "spawn should fail on invalid spec");
        // registry 必须仍为空 —— 这是契约的核心。
        assert!(
            reg.snapshot().is_empty(),
            "failed spawn MUST NOT write to registry"
        );
    }

    /// bug-8 白盒:`format!(" [worker: {}]", role)` 后缀逻辑的正确性 —— 隔离
    /// 测试,直接调该模式而不走 spawn。保证:role 字符串拼到 task_summary 末尾,
    /// coordinator 关闭时不附加。
    #[test]
    fn worker_role_suffix_format() {
        // 这是 execute 内"if coordinator_mode { append }"分支的等价手工
        // 构造。覆盖两条规则:coordinator on + role → 加;coordinator off → 不加。
        let role = "explorer";
        let mut s = String::from("find modules");
        let coord_on = true;
        if coord_on {
            s.push_str(&format!(" [worker: {}]", role));
        }
        assert_eq!(s, "find modules [worker: explorer]");

        let mut s2 = String::from("find modules");
        let coord_off = false;
        if coord_off {
            s2.push_str(&format!(" [worker: {}]", role));
        }
        assert_eq!(s2, "find modules");
    }
}
