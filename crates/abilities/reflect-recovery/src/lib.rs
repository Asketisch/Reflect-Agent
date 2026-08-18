//! `reflect-recovery` — post-compact 上下文恢复三件套(v1.1.0 Phase 6 P0)。
//!
//! 把三类"上一轮丢失、但下一轮 LLM 仍需要"的上下文在 `pre_loop` 阶段
//! 恢复成 `<system-reminder>` 注入到消息流:
//!
//! - **Active File Context Recovery**(`active_files`):压缩后自动重读
//!   最近 write / replace / edit_file 过的文件内容(50k token 预算,
//!   10 文件上限,5k / 文件)。
//! - **Subagent Registry**(`subagent_registry`):已完成的子代理调用
//!   记录,LLM 看到后不会再 spawn 重复子代理。
//! - **Session Memory Notes**:`reflect-notes` 的 FIFO 笔记镜像
//!   (Phase A 已实现,本 crate 只负责渲染)。
//!
//! ## 渲染顺序
//!
//! `recovery_meta_to_messages()` 按 `MetaKind` 枚举顺序渲染:
//! `ActiveFiles` → `SubagentRegistry` → `SessionMemory`。
//!
//! ## 单实例去重
//!
//! 每个 `RecoveryEntry` 有 `dedup_key`,渲染时按 key 折叠保留最后一条;
//! 空 key 视为"非单实例"(留给未来每文件 / 每 note 的多实例场景)。

pub mod active_files;
pub mod render;
pub mod subagent;

pub use active_files::{
    ACTIVE_FILES_MAX_FILES, ACTIVE_FILES_PER_FILE_TOKEN_CAP, ACTIVE_FILES_TOKEN_BUDGET,
    ActiveFileRecovery, DEFAULT_WRITE_TOOLS,
};
pub use render::{MetaKind, RecoveryEntry, recovery_meta_to_messages};
pub use subagent::{
    SUBAGENT_REGISTRY_CAP, SUBAGENT_RESULT_SUMMARY_MAX, SUBAGENT_TASK_SUMMARY_MAX,
    SubagentRegistry, SubagentRegistryEntry,
};
