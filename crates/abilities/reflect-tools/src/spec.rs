//! `ToolSpec` —— 工具的模型侧描述。

use serde::{Deserialize, Serialize};

use reflect_protocol::PermissionMode;

/// 兼容 OpenAI / Anthropic 的可调用工具描述。
///
/// M1:只有 `Function`(标准 JSON-Schema tools 形态)。
/// M2+:增加 `ProviderBuiltin` 与 `Mcp` 变体,用于 provider 原生与 MCP 工具。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        description: String,
        parameters: serde_json::Value,
        /// 工具所需的权限(M3+;默认 `Auto`)。
        #[serde(default)]
        required_permission: PermissionMode,
    },
}

impl ToolSpec {
    /// 工具名(仅对 `Function` 有效,其他变体会 panic)。
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Function { name, .. } => name,
        }
    }

    /// 实际要求的权限。
    pub fn required_permission(&self) -> PermissionMode {
        match self {
            ToolSpec::Function {
                required_permission,
                ..
            } => *required_permission,
        }
    }
}

// v1.6:按模型匹配的 description 覆盖。trait 定义在 reflect-prompt,
// impl 放本 crate —— reflect-tools 已依赖 reflect-prompt,反向 impl
// 会成环,孤儿规则也要求 trait 或类型在本 crate。
impl reflect_prompt::DescriptionSource for ToolSpec {
    fn name(&self) -> &str {
        ToolSpec::name(self)
    }
    fn set_description(&mut self, description: String) {
        // 单变体 enum,直接解构(若未来加变体需补 match 臂)。
        let ToolSpec::Function {
            description: slot, ..
        } = self;
        *slot = description;
    }
}
