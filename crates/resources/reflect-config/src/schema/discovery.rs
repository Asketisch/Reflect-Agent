//! `[model_discovery]` 段 —— 模型清单发现与免费模型判定规则。
//!
//! ## 为什么需要这一段
//!
//! 免费模型池的成员集合由**上游**决定,随时可能增删。因此模型 id 不能
//! 写死到配置里 —— 必须从 provider 的清单端点实时拉取。本段配置的是
//! **「怎么发现」与「怎么判免费」**,不是模型清单本身。
//!
//! ## 用法
//!
//! ```toml
//! [active]
//! provider = "openai"
//!
//! [openai]
//! api_key = ""                                              # 免 Key 端点
//! base_url = "https://api.kilo.ai/api/gateway"
//! model = "free"                                            # 运行时从清单里挑
//!
//! [model_discovery]
//! enabled = true
//! timeout_secs = 20
//! prefer = "code"                                           # 偏好含该子串的模型
//! always_free = ["union-alpha", "big-pickle"]               # 不带 free 标记的例外
//! free_id_patterns = ["-free", ":free", "/free"]             # id 模式
//! ```

use serde::{Deserialize, Serialize};

use reflect_llm::FreeModelRules;

/// `[model_discovery]` 段。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelDiscoverySection {
    /// 关掉后,`model = "free"` 直接报错 —— 明确失败优于模糊兜底。
    ///
    /// 缺省 `true`:发现能力默认可用。
    #[serde(default = "default_discovery_enabled")]
    pub enabled: bool,
    /// 清单接口超时(秒)。缺省由调用方决定(建议 20s)。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 选模型时的 id 子串偏好(可空)。
    ///
    /// 是**偏好**而非硬过滤:优先挑 id 含该子串的免费模型;一个都没有时
    /// 回落成任意免费模型。
    #[serde(default)]
    pub prefer: Option<String>,
    /// 显式白名单:命中即视为免费(精确匹配 id)。
    ///
    /// 留空则用 `reflect_llm::DEFAULT_ALWAYS_FREE`。
    #[serde(default)]
    pub always_free: Vec<String>,
    /// id 模式:命中即视为免费(子串匹配)。
    ///
    /// 留空则用 `reflect_llm::DEFAULT_FREE_ID_PATTERNS`。
    #[serde(default)]
    pub free_id_patterns: Vec<String>,
}

fn default_discovery_enabled() -> bool {
    true
}

impl Default for ModelDiscoverySection {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: None,
            prefer: None,
            always_free: Vec::new(),
            free_id_patterns: Vec::new(),
        }
    }
}

impl ModelDiscoverySection {
    /// 构造免费判定规则(留空项回落到内置默认)。
    pub fn rules(&self) -> FreeModelRules {
        FreeModelRules::new(self.always_free.clone(), self.free_id_patterns.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReflectConfig;

    #[test]
    fn 缺省时发现能力开启() {
        let cfg = ReflectConfig::default();
        let section = cfg.model_discovery.clone().unwrap_or_default();
        assert!(section.enabled);
    }

    #[test]
    fn 段解析各字段() {
        let toml = r#"
            [model_discovery]
            enabled = false
            timeout_secs = 30
            prefer = "code"
            always_free = ["x-free"]
            free_id_patterns = ["-free"]
        "#;
        let cfg: crate::ReflectConfig = toml::from_str(toml).unwrap();
        let d = cfg.model_discovery.expect("model_discovery 段应被解析");
        assert!(!d.enabled);
        assert_eq!(d.timeout_secs, Some(30));
        assert_eq!(d.prefer.as_deref(), Some("code"));
        assert_eq!(d.always_free, vec!["x-free".to_string()]);
        assert_eq!(d.free_id_patterns, vec!["-free".to_string()]);
    }

    #[test]
    fn 规则留空时回落到内置默认() {
        let rules = ModelDiscoverySection::default().rules();
        assert!(rules.free_id_patterns.iter().any(|p| p == "-free"));
        assert!(rules.always_free.iter().any(|id| id == "union-alpha"));
    }
}
