//! `AstAction` — `ast` tool 的 action 枚举。
//!
//! 镜像 `reflect_lsp::methods::LspAction` 的设计:每个 variant 对应一个
//! 结构化操作,`is_read_only()` / `required_permission()` /
//! `is_concurrency_safe()` per-action 表决定权限模型与并发模型。P0 阶段
//! 所有 action 走 `Auto` 与 `false`(串行)以避免 LLM 早期试探就触发
//! approval flow;P3 在 `ToolContext::approval` refactor 完成后切到
//! per-action 表(`replace` / `rename_symbol` → `Prompt`)。

use reflect_protocol::PermissionMode;
use serde::Deserialize;

/// LLM 在 `ast` tool 调用里填的 `action` 字段。
///
/// v1 4 个 variant:P0 仅 `ListLanguages` 真正可用,其余在 P1 / P2 / P3
/// 增量实现。`serde(rename_all = "snake_case")` 把 variant 序列化成
/// `"list_languages"` / `"search"` 等 snake_case,LLM 在 tool call JSON
/// 里直接填这些字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AstAction {
    /// 列出当前 build 支持的所有 grammar。P0 实现。
    ListLanguages,
    /// 按 `pattern` 在 `path` 下找结构化匹配(只读)。P1+ 实现。
    Search,
    /// 用 `replacement` 改写每个匹配节点(写)。P3 实现。
    Replace,
    /// 单文件内 symbol 重命名(写)。P3 实现。
    RenameSymbol,
}

impl AstAction {
    /// 当前 action 是否只读(不修改文件、不发副作用)。
    ///
    /// `ListLanguages` / `Search` 是只读;`Replace` / `RenameSymbol` 会
    /// 写盘。P0 阶段所有 action 都是 P1/P2/P3 stub,所以全 `true` —— 一旦
    /// P3 启用,这里必须切 `Replace` / `RenameSymbol` 为 `false`。
    pub fn is_read_only(self) -> bool {
        match self {
            AstAction::ListLanguages | AstAction::Search => true,
            AstAction::Replace | AstAction::RenameSymbol => false,
        }
    }

    /// Per-action 权限表(P0 stub)。
    ///
    /// 当前所有 action 都是 stub,全 `Auto` —— LLM 试探 `ast search` 等
    /// 不会触发 approval modal,方便 P1+ 增量验证。P3 完成
    /// `ToolContext::approval` refactor 后切到正确表:`Replace` /
    /// `RenameSymbol` → `Prompt`(mutation 必须经 approval gate)。
    pub fn required_permission(self) -> PermissionMode {
        match self {
            AstAction::ListLanguages
            | AstAction::Search
            | AstAction::Replace
            | AstAction::RenameSymbol => PermissionMode::Auto,
        }
    }

    /// Per-action 并发安全标记(P0 stub)。
    ///
    /// P3 实现 `Replace` / `RenameSymbol` 时,前者按文件写盘(`false`)、
    /// 后者按文件写盘(`false`)。`ListLanguages` / `Search` 是只读 +
    /// 文件无关(`true`)。
    pub fn is_concurrency_safe(self) -> bool {
        match self {
            AstAction::ListLanguages | AstAction::Search => true,
            AstAction::Replace | AstAction::RenameSymbol => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serde_roundtrip_all_variants() {
        for (raw, want) in [
            (r#""list_languages""#, AstAction::ListLanguages),
            (r#""search""#, AstAction::Search),
            (r#""replace""#, AstAction::Replace),
            (r#""rename_symbol""#, AstAction::RenameSymbol),
        ] {
            let parsed: AstAction = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, want);
        }
    }

    #[test]
    fn invalid_action_errors() {
        let r: Result<AstAction, _> = serde_json::from_str(r#""unknown_action""#);
        assert!(r.is_err());
    }

    #[test]
    fn p0_stub_returns_auto_and_read_only_consistent() {
        // P0 阶段所有 action 走 stub:全 Auto + 全 read_only = true。
        // 这个测试是 P3 refactor 的 sentinel —— 一旦失败,说明有人改
        // 了 is_read_only 或 required_permission 但忘了同步 queue 路由。
        for action in [
            AstAction::ListLanguages,
            AstAction::Search,
            AstAction::Replace,
            AstAction::RenameSymbol,
        ] {
            assert_eq!(
                action.required_permission(),
                PermissionMode::Auto,
                "{action:?} should be Auto in P0"
            );
        }
        // P3 改写时,Replace / RenameSymbol 必须切到 false,这里只保证
        // ListLanguages / Search 是 true。
        assert!(AstAction::ListLanguages.is_read_only());
        assert!(AstAction::Search.is_read_only());
    }
}
