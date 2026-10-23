//! `reflect-permissions` —— 持久化工具权限规则 + 审批 resolver(v1.x S5a)。
//!
//! 三块组件,各司其职:
//!
//! - [`rules`]:纯数据结构 `PermissionRule { tool, action }` + `RuleMatch`
//!   枚举(Allow / Deny / Ask / NoMatch) + `evaluate(rules, tool)` 求值函数。
//!   不碰 IO,易测。
//! - [`store`]:trait `PermissionStore`(list / add / remove)+ 两个实现
//!   ——`FilePermissionStore` 走 `~/.reflect/permissions.toml`(TOML 格式,
//!   可手编辑 + `tracing` 友好);`InMemoryPermissionStore` 给测试用。
//! - [`resolver`]:trait `PermissionResolver::resolve(tool) -> RuleMatch` +
//!   `StorePermissionResolver` 实现,封装 store + 错误降级(列表失败 →
//!   `NoMatch`,让上层走默认审批 modal,**不**因为 rules 读不出就 deny
//!   用户工作流)。
//!
//! v1.x 简化:
//! - ~~`tool` 字段精确匹配(无 glob)~~(P2 `permission-rules` 已补:
//!   `matcher` 模块支持 `tool_glob`(如 `Web*`)与 bash 专用
//!   `shell_pattern` 的上下文感知匹配);scope(session/user/project)
//!   留 v2。
//! - 没有 rule precedence 概念 —— 第一条匹配即返回(用户 add 的顺序
//!   隐式定 precedence)。
//! - 持久化只在 HOME base,无 `$REFLECT_HOME` 支持(留给 reflect-config
//!   统一引入)。
//!
//! ApprovalGate 集成点在 `reflect-tools::approval::ApprovalGate` —— 启动
//! 期把 `Arc<dyn PermissionResolver>` 灌进 gate,`ask_tool` 短路
//! Allow/Deny(详见 S5a.2 commit)。

pub mod matcher;
pub mod resolver;
pub mod rules;
pub mod store;
pub mod yolo;

pub use matcher::evaluate_with_context;
pub use resolver::{PermissionResolver, StorePermissionResolver};
pub use rules::{PermissionAction, PermissionRule, RuleMatch, evaluate};
pub use store::{
    ChainedPermissionStore, FilePermissionStore, InMemoryPermissionStore, PermissionStore,
    PermissionsError,
};
pub use yolo::{HeuristicYoloClassifier, YoloClassification, YoloClassifier, YoloSuggestion};
