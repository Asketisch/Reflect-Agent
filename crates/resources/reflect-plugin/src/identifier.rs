//! 插件 ID 与 marketplace 名称的解析与校验。
//!
//! ID 格式 = `<plugin>@<marketplace>`,严格 kebab-case 且
//! 不允许大小写字母 —— 强制 ASCII 便于跨平台、跨 shell 处理。
//!
//! 保留名:`inline`(本地路径直接安装,不绑定 marketplace)、
//! `builtin`(reflect 自带内建插件的命名空间)。

use crate::errors::{PluginError, Result};
use serde::{Deserialize, Serialize};

/// 插件 ID 命名正则 —— 大小写不敏感。
///
/// 形态:`<plugin>@<marketplace>`,两段均匹配 `^[a-z0-9][-a-z0-9._]*$`
/// (case-insensitive)。解析成功后统一 lower-case 存储,便于
/// `BTreeMap` 主键稳定比较。
const ID_RE_STR: &str = r"(?i)^[a-z0-9][-a-z0-9._]*@[a-z0-9][-a-z0-9._]*$";

/// Marketplace 保留名 —— 不允许 marketplace 自称这些名字。
pub const RESERVED_MARKETPLACE_NAMES: &[&str] = &["inline", "builtin"];

/// 插件唯一标识 = `<plugin>@<marketplace>`。
///
/// `#derive(Serialize, Deserialize)]` 把 plugin_id 序列化为单一字符串,
/// 便于在 `installed_plugins.json`、`settings.json` 与 protocol 事件里
/// 复用同一形态。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginId(String);

impl PluginId {
    /// 从字符串构造并校验。
    pub fn parse(s: &str) -> Result<Self> {
        validate_id_str(s)?;
        Ok(Self(s.to_ascii_lowercase()))
    }

    /// 从 `(name, marketplace)` 构造,任一段会做校验。
    pub fn new(name: &str, marketplace: &str) -> Result<Self> {
        validate_segment(name, "plugin name")?;
        validate_marketplace(marketplace)?;
        Ok(Self(format!(
            "{}@{}",
            name.to_ascii_lowercase(),
            marketplace.to_ascii_lowercase()
        )))
    }

    /// CLI / 用户输入用的宽松构造 —— 允许保留 marketplace 名(`inline` /
    /// `builtin`),段级走 `validate_segment`,marketplace 段只做最基础的
    /// kebab-case + 小写化校验。
    ///
    /// 用户键入 `demo@inline` 应被接受,
    /// 因为 `inline` 是合法 marketplace(指本地 install),不是用户取的名字。
    pub fn parse_user_input(s: &str) -> Result<Self> {
        let (name, marketplace) = s
            .split_once('@')
            .ok_or_else(|| PluginError::InvalidPluginId(s.to_string()))?;
        validate_segment(name, "plugin name")?;
        if marketplace.is_empty() {
            return Err(PluginError::InvalidPluginId(s.to_string()));
        }
        if !marketplace.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        }) {
            return Err(PluginError::Validation(format!(
                "marketplace '{marketplace}' must be lowercase kebab-case"
            )));
        }
        Ok(Self(format!(
            "{}@{}",
            name.to_ascii_lowercase(),
            marketplace.to_ascii_lowercase()
        )))
    }

    /// 取得 plugin 名段。
    pub fn name(&self) -> &str {
        self.0
            .split('@')
            .next()
            .expect("validated ID always contains '@'")
    }

    /// 取得 marketplace 名段。
    pub fn marketplace(&self) -> &str {
        self.0
            .split('@')
            .nth(1)
            .expect("validated ID always contains '@'")
    }

    /// 取得底层字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 构造一个 `@inline` ID,表示"本地路径直接安装"。
    ///
    /// 不走 `validate_marketplace`(会拒绝保留名),直接构造 —— 这是
    /// `inline` 关键字的特例用法。
    pub fn inline(name: &str) -> Result<Self> {
        validate_segment(name, "plugin name")?;
        Ok(Self(format!("{}@inline", name.to_ascii_lowercase())))
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Marketplace 名称(独立于 PluginId 的命名类型,作 marketplace manifest 的主键)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketplaceName(String);

impl MarketplaceName {
    pub fn parse(s: &str) -> Result<Self> {
        validate_marketplace(s)?;
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn inline() -> Self {
        Self("inline".to_string())
    }

    pub fn builtin() -> Self {
        Self("builtin".to_string())
    }

    pub fn is_inline(&self) -> bool {
        self.0 == "inline"
    }

    pub fn is_builtin(&self) -> bool {
        self.0 == "builtin"
    }
}

impl std::fmt::Display for MarketplaceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── 内部校验 ─────────────────────────────────────────────────────────

fn validate_id_str(s: &str) -> Result<()> {
    let re = regex::Regex::new(ID_RE_STR).expect("plugin ID regex is a literal and must compile");
    if !re.is_match(s) {
        return Err(PluginError::InvalidPluginId(s.to_string()));
    }
    // split 后做段级校验,捕获 marketplace 是否撞保留名。
    let (name, marketplace) = s
        .split_once('@')
        .ok_or_else(|| PluginError::InvalidPluginId(s.to_string()))?;
    validate_segment(name, "plugin name")?;
    validate_marketplace(marketplace)?;
    Ok(())
}

fn validate_segment(s: &str, kind: &str) -> Result<()> {
    // case-insensitive 与 ID_RE_STR 一致。
    let re = regex::Regex::new(r"(?i)^[a-z0-9][-a-z0-9._]*$").expect("segment regex is a literal");
    if !re.is_match(s) {
        return Err(PluginError::Validation(format!(
            "{kind} 不符合 kebab-case: {s:?}"
        )));
    }
    Ok(())
}

fn validate_marketplace(s: &str) -> Result<()> {
    validate_segment(s, "marketplace")?;
    let lower = s.to_ascii_lowercase();
    if RESERVED_MARKETPLACE_NAMES.contains(&lower.as_str()) {
        return Err(PluginError::InvalidMarketplaceName(format!(
            "{s:?} 是保留 marketplace 名"
        )));
    }
    Ok(())
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_id() {
        let id = PluginId::parse("code-formatter@anthropic-tools").unwrap();
        assert_eq!(id.name(), "code-formatter");
        assert_eq!(id.marketplace(), "anthropic-tools");
        assert_eq!(id.to_string(), "code-formatter@anthropic-tools");
    }

    #[test]
    fn parse_normalizes_case() {
        let id = PluginId::parse("Code-Formatter@Anthropic-Tools").unwrap();
        assert_eq!(id.as_str(), "code-formatter@anthropic-tools");
    }

    #[test]
    fn parse_accepts_and_normalizes_uppercase() {
        // 正则 `(?i)` 大小写不敏感;成功后统一 lower-case 存储。
        // `i` flag 保证主键稳定排序要求所有 ID 同形。
        let id = PluginId::parse("BadName@MarketPlace").unwrap();
        assert_eq!(id.as_str(), "badname@marketplace");
    }

    #[test]
    fn parse_rejects_unicode_or_special_chars() {
        // kebab-case 仅允许 ASCII;Unicode 字符被拒绝。
        assert!(matches!(
            PluginId::parse("foo-bar@市场"),
            Err(PluginError::InvalidPluginId(_))
        ));
        assert!(matches!(
            PluginId::parse("foo bar@marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
        assert!(matches!(
            PluginId::parse("foo!bar@marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
    }

    #[test]
    fn parse_rejects_space() {
        assert!(matches!(
            PluginId::parse("foo bar@marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
    }

    #[test]
    fn parse_rejects_slash() {
        assert!(matches!(
            PluginId::parse("foo/bar@marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
    }

    #[test]
    fn parse_rejects_no_at_sign() {
        assert!(matches!(
            PluginId::parse("foo-marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
    }

    #[test]
    fn parse_rejects_multiple_at_signs() {
        // Reflect 严格校验要求恰好一段 @,避免歧义。
        assert!(matches!(
            PluginId::parse("foo@bar@baz"),
            Err(PluginError::InvalidPluginId(_))
        ));
    }

    #[test]
    fn parse_rejects_reserved_marketplace() {
        assert!(matches!(
            PluginId::parse("foo@inline"),
            Err(PluginError::InvalidMarketplaceName(_))
        ));
        assert!(matches!(
            PluginId::parse("foo@builtin"),
            Err(PluginError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn parse_rejects_leading_dash() {
        // 外层 ID 正则要求首字符为 a-z/0-9,所以 `-foo@...` 命中
        // `InvalidPluginId` 而非 `Validation`。
        assert!(matches!(
            PluginId::parse("-foo@marketplace"),
            Err(PluginError::InvalidPluginId(_))
        ));
        // 段级校验:`-foo` 单独传给 `validate_segment` 会命中 `Validation`。
        assert!(matches!(
            PluginId::new("-foo", "marketplace"),
            Err(PluginError::Validation(_))
        ));
    }

    #[test]
    fn new_from_segments() {
        let id = PluginId::new("foo", "bar").unwrap();
        assert_eq!(id.as_str(), "foo@bar");
    }

    #[test]
    fn inline_constructor() {
        let id = PluginId::inline("local-plugin").unwrap();
        assert_eq!(id.marketplace(), "inline");
    }

    #[test]
    fn marketplace_name_reserved() {
        assert!(MarketplaceName::parse("inline").is_err());
        assert!(MarketplaceName::parse("builtin").is_err());
        assert!(MarketplaceName::parse("good-name").is_ok());
    }

    #[test]
    fn marketplace_name_inline_helper() {
        let n = MarketplaceName::inline();
        assert!(n.is_inline());
        assert!(!n.is_builtin());
    }

    #[test]
    fn serde_round_trip() {
        let id = PluginId::parse("foo@bar").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"foo@bar\"");
        let back: PluginId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn plugin_id_ordering() {
        // ID 排序稳定 —— 用于 installed_plugins.json 的 BTreeMap 主键。
        let a = PluginId::parse("aaa@zzz").unwrap();
        let b = PluginId::parse("bbb@aaa").unwrap();
        assert!(a < b);
    }
}
