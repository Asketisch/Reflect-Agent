#![allow(clippy::derivable_impls)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::io_other_error)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::manual_div_ceil)]
//! `reflect` —— 顶层库门面。
//!
//! 支持两种访问方式:
//!
//! 1. **扁平 re-export**(扁平重导出)—— 适合希望显式写出类型名的深度用户:
//!
//!    ```ignore
//!    use reflect::{AgentThread, Submission, Op, Event, EventMsg, ModelRegistry};
//!    // 上方为常用类型的扁平 re-export 路径
//!    ```
//!
//! 2. **Builder**(构建器)—— 一行启动 + 合理默认:
//!
//!    ```ignore
//!    let agent = Reflect::builder("openai/gpt-4o")   // 指定模型
//!        .workspace(".")                              // 指定工作目录
//!        .with_defaults()?    // 压缩器 + 记忆 + 技能 + JSONL recorder
//!        .build()?;
//!    let mut stream = agent.submit(Submission::user_input("hello"));  // 提交输入
//!    while let Some(event) = stream.next().await { /* 消费事件 */ }
//!    ```
//!
//! 门面映射关系见 `docs/architecture.md §8`。

pub mod builder;
pub mod stream;

// ── Core(保持原状)─────────────────────────────────────────────────────────
pub use reflect_core::{AgentConfig, AgentThread, TurnHandle};

// ── LLM ────────────────────────────────────────────────────────────────────
pub use reflect_llm::{
    AnthropicClient, AnthropicConfig, Capabilities, ChatEvent, ChatMessage, ChatRequest,
    ContentBlock as LlmContentBlock, CredentialPool, LlmError, ModelClient, ModelRegistry,
    OllamaClient, OllamaConfig, OpenAIClient, OpenAIConfig, PoolEntry, ProviderKind,
    SharedModelRegistry, SystemBlocks, ThinkingConfig, providers,
};

// ── 协议(Protocol)───────────────────────────────────────────────────────
pub use reflect_protocol::{
    AbortReason, AgentMessage, AgentMessageDelta, ApprovalKind, ApprovalPolicy,
    ApprovalRequestEvent, ContentBlock, ContextCompactedEvent, ContextCompactedStrategy,
    ErrorEvent, Event, EventMsg, MessageRole, NullRecorder, Op, ProtocolError, ReviewDecision,
    RiskLevel, RolloutRecord, RolloutRecorder, SandboxPolicy, SessionConfiguredEvent, SessionInfo,
    StreamErrorEvent, Submission, ThreadId, TokenCountEvent, TokenUsage, ToolCallBeginEvent,
    ToolCallEndEvent, ToolOutput, TurnAbortedEvent, TurnCompleteEvent, TurnId, TurnStartedEvent,
    TurnStatus, UserInputItem,
};

// ── Tools ──────────────────────────────────────────────────────────────────
pub use reflect_protocol::PermissionMode;
pub use reflect_tools::{
    Tool, ToolContext, ToolError, ToolOutput as ToolOutputType, ToolRegistry, ToolSpec,
};

// ── Hooks(M6 新暴露)────────────────────────────────────────────────────────
pub use reflect_hooks::{
    Hook, HookAbortSignal, HookContext, HookDecision, HookEngine, HookError, HookEvent,
    HookEventKind, SystemMessage,
};

// ── Memory(新暴露)─────────────────────────────────────────────────────────
pub use reflect_memory::{FileMemoryStore, InMemoryStore, MemoryScope, MemoryStore};

// ── Skills(新暴露)─────────────────────────────────────────────────────────
pub use reflect_skills::SkillsCatalog;

// ── Agent definitions(新暴露)──────────────────────────────────────────────
pub use reflect_agent_def::AgentDefinition;

// ── Compact(新暴露)─────────────────────────────────────────────────────────
pub use reflect_compact::Compactor;

// ── Rollout(新暴露)─────────────────────────────────────────────────────────
pub use reflect_rollout::JsonlRolloutWriter;

// ── Subagent(新暴露)───────────────────────────────────────────────────────
pub use reflect_subagent::{CallSubAgentTool, DataTransferConfig, SubAgentFactory, SubAgentSpec};

// ── 讨论(Discussion,M9 首次暴露,v0.2.3 扩展)──────────────────────────────
// 顶层 re-export:核心 facade (M9) + LLM 实战集成所需的高级 API (v0.2.3)。
// `comm_tools` / `runtime` / `orchestrator` 仍走 `reflect_discussion::*`,
// 避免 facade 臃肿;advanced 用户也可直接 import 完整路径。
pub use reflect_discussion::{
    // 高级 API (v0.2.3 —— 真实 LLM 集成所需)
    AgentId,
    AgentSection,
    // 核心 facade (M9)
    DiscussionConfig,
    DiscussionId,
    DiscussionMode,
    DiscussionOrchestrator,
    DiscussionResult,
    MessageBus,
    MessageId,
    MessageKind,
    OrchestratorEvent,
    // v0.2.3 LLM wiring helper:把 AgentSection[] 编译成 LlmContext,
    // 返回 prompt_for_closure 喂给 DiscussionOrchestrator::run。
    build_context,
    prompt_for_closure,
};

// ── Builder 外观(Builder facade)──────────────────────────────────────
pub use builder::ReflectBuilder;

/// 便捷别名:用一条链式调用构造 `Reflect`。`Reflect` 即
/// `reflect::Reflect`,包装一个 `AgentThread`。
pub type Reflect = builder::Reflect;
