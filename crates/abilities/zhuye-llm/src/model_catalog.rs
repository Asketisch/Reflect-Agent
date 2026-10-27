//! `model_catalog` —— 模型清单条目 + 免费模型判定规则。
//!
//! ## 为什么需要它
//!
//! 免费模型池的成员集合由**上游**决定,随时可能增删(上游自己的文档原话:
//! 「模型集合与额度政策由上游决定,随时可能变更」)。因此模型 id **不能写死**
//! 到配置或代码里 —— 必须从 provider 的清单端点实时拉取。
//!
//! 本模块提供两件事:
//! - [`ModelInfo`] —— 一条清单条目(provider 披露多少就存多少,缺失即 `None`);
//! - [`FreeModelRules`] —— **判定规则**(不是模型清单!):怎么把一条条目
//!   判成「免费」,以及怎么从一堆条目里确定性地挑一个。
//!
//! ## 规则与清单的分工
//!
//! 规则只描述**怎么判**,不描述**哪些模型免费**。实测三种清单方言的披露
//! 能力差别很大,单靠任何一种字段都覆盖不全:
//!
//! | 方言 | 条目字段 | 免费信息来源 |
//! |---|---|---|
//! | Kilo 系(`isFree` / `pricing`) | 丰富 | `isFree` 显式标记(权威) |
//! | OpenAI 原生(`{id, object, created}`) | 只有 id | id 规则 + 白名单 |
//! | 聚合网关(`{id, context_window, x_ofm_efforts}`) | 中等 | id 规则 + 白名单 |
//!
//! 所以判定走**三通道**:显式标记 → 白名单 → id 模式。任一通道命中即成立。

use serde::{Deserialize, Serialize};

/// 一条模型清单条目(来自 provider 的模型清单端点)。
///
/// 字段全部可选:provider 披露多少就填多少,**缺失不编造**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// 模型 id(不含 provider 前缀)。如 `"cohere/north-mini-code:free"`。
    pub id: String,
    /// 上下文窗口(token)。provider 未披露则 `None`。
    pub context_window: Option<u32>,
    /// provider **显式**声明的免费标记。
    ///
    /// `Some(true)` / `Some(false)` = provider 明说;
    /// `None` = provider 没表态,需由 [`FreeModelRules`] 按规则推断。
    #[serde(default)]
    pub is_free: Option<bool>,
    /// provider 声明的思考档位菜单(如聚合网关的 `x_ofm_efforts`)。
    #[serde(default)]
    pub efforts: Option<Vec<String>>,
    /// 清单条目的归属方(provider 原样透传的 `owned_by`)。
    #[serde(default)]
    pub owned_by: Option<String>,
}

impl ModelInfo {
    /// 构造一条只有 id 的条目(其余字段留空)。
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }
}

/// 免费模型的**判定规则**。
///
/// ⚠️ 这里存的是「怎么判」,**不是模型清单**。`always_free` 里的少数 id 是
/// 「上游既不标 `isFree`、id 也不带 free 标记」的例外,可通过配置覆盖。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeModelRules {
    /// 显式白名单:命中即视为免费(精确匹配 id)。
    pub always_free: Vec<String>,
    /// id 模式:命中即视为免费(子串匹配)。
    pub free_id_patterns: Vec<String>,
}

impl Default for FreeModelRules {
    fn default() -> Self {
        Self {
            always_free: DEFAULT_ALWAYS_FREE
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            free_id_patterns: DEFAULT_FREE_ID_PATTERNS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

/// 默认白名单 —— 上游既不标免费标记、id 也不带 free 后缀的少数免费 id。
///
/// 这几个 id 来自免费车道的上游约定(其 `ALWAYS_FREE` 常量),**可以整体
/// 覆盖**:配置里写 `[model_discovery] always_free = [...]` 即可。
pub const DEFAULT_ALWAYS_FREE: &[&str] = &["union-alpha", "big-pickle", "space-bunny-free"];

/// 默认 id 模式 —— 命中即视为免费。
///
/// 覆盖三种常见命名:`xxx-free`(连字符)、`xxx:free`(冒号,OpenRouter 风格)、
/// `vendor/xxx/free`(斜杠,自动路由风格)。
pub const DEFAULT_FREE_ID_PATTERNS: &[&str] = &["-free", ":free", "/free"];

impl FreeModelRules {
    /// 用自定义白名单与模式构造(两者皆空则用默认)。
    pub fn new(always_free: Vec<String>, free_id_patterns: Vec<String>) -> Self {
        Self {
            always_free: if always_free.is_empty() {
                DEFAULT_ALWAYS_FREE
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                always_free
            },
            free_id_patterns: if free_id_patterns.is_empty() {
                DEFAULT_FREE_ID_PATTERNS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                free_id_patterns
            },
        }
    }

    /// 判定一条条目是否免费。
    ///
    /// 返回 `Some(true)` = 免费,`Some(false)` = 明确不免费,
    /// `None` = **未知**(provider 没表态且规则也没命中)。
    ///
    /// 判定顺序(先到先得):
    /// 1. provider 显式标记 `is_free` —— 权威,覆盖一切规则;
    /// 2. 命中白名单 `always_free`;
    /// 3. 命中任一 id 模式 `free_id_patterns`;
    /// 4. 否则未知。
    pub fn classify(&self, info: &ModelInfo) -> Option<bool> {
        if let Some(flag) = info.is_free {
            return Some(flag);
        }
        if self.always_free.iter().any(|id| id == &info.id) {
            return Some(true);
        }
        if self
            .free_id_patterns
            .iter()
            .any(|pattern| !pattern.is_empty() && info.id.contains(pattern))
        {
            return Some(true);
        }
        None
    }

    /// 从清单里**确定性地**挑一个免费模型。
    ///
    /// 确定性 = 同样的清单 + 同样的规则 → 同样的结果(按 id 字典序取首个),
    /// 这样 live 测试与演练脚本可复现。
    ///
    /// `prefer` 是**偏好**而非硬过滤:优先挑 id 含该子串的免费模型;一个都
    /// 没有时回落成任意免费模型(而不是失败)。
    pub fn select<'a>(
        &self,
        models: &'a [ModelInfo],
        prefer: Option<&str>,
    ) -> Option<&'a ModelInfo> {
        let free: Vec<&ModelInfo> = models
            .iter()
            .filter(|m| self.classify(m) == Some(true))
            .collect();
        if free.is_empty() {
            return None;
        }
        if let Some(prefer) = prefer.map(str::trim).filter(|p| !p.is_empty()) {
            let matched: Vec<&ModelInfo> = free
                .iter()
                .copied()
                .filter(|m| m.id.contains(prefer))
                .collect();
            if !matched.is_empty() {
                return first_by_id(&matched);
            }
        }
        first_by_id(&free)
    }
}

/// 按 id 字典序取首个(确定性挑选的统一入口)。
fn first_by_id<'a>(items: &[&'a ModelInfo]) -> Option<&'a ModelInfo> {
    items.iter().copied().min_by(|a, b| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 显式标记优先于一切规则() {
        let rules = FreeModelRules::default();
        let info = ModelInfo {
            id: "some-paid-model".into(),
            is_free: Some(false),
            ..Default::default()
        };
        assert_eq!(rules.classify(&info), Some(false));
    }

    #[test]
    fn id命中负斜杠模式判为免费() {
        let rules = FreeModelRules::default();
        assert_eq!(rules.classify(&ModelInfo::new("beam-free")), Some(true));
        assert_eq!(
            rules.classify(&ModelInfo::new("cohere/north-mini-code:free")),
            Some(true)
        );
        assert_eq!(
            rules.classify(&ModelInfo::new("kilo-auto/free")),
            Some(true)
        );
    }

    #[test]
    fn 命中白名单判为免费() {
        let rules = FreeModelRules::default();
        assert_eq!(rules.classify(&ModelInfo::new("union-alpha")), Some(true));
        assert_eq!(rules.classify(&ModelInfo::new("big-pickle")), Some(true));
    }

    #[test]
    fn 未命中任何规则为未知() {
        let rules = FreeModelRules::default();
        assert_eq!(rules.classify(&ModelInfo::new("gpt-4o")), None);
        assert_eq!(
            rules.classify(&ModelInfo::new("stealth/glyph-cluster")),
            None
        );
    }

    #[test]
    fn 自定义规则覆盖默认() {
        let rules = FreeModelRules::new(vec!["my-free".into()], vec![".-free".into()]);
        assert_eq!(rules.classify(&ModelInfo::new("my-free")), Some(true));
        assert_eq!(rules.classify(&ModelInfo::new("beam-free")), None);
    }

    #[test]
    fn 选择按id字典序确定() {
        let rules = FreeModelRules::default();
        let models = vec![
            ModelInfo::new("zeta-free"),
            ModelInfo::new("alpha-free"),
            ModelInfo::new("mid-free"),
        ];
        let picked = rules.select(&models, None).expect("应选中一个");
        assert_eq!(picked.id, "alpha-free");
    }

    #[test]
    fn 偏好命中时优先选偏好() {
        let rules = FreeModelRules::default();
        let models = vec![
            ModelInfo::new("alpha-free"),
            ModelInfo::new("cohere/north-mini-code:free"),
        ];
        let picked = rules.select(&models, Some("code")).expect("应选中一个");
        assert_eq!(picked.id, "cohere/north-mini-code:free");
    }

    #[test]
    fn 偏好无命中时回落任意免费模型() {
        let rules = FreeModelRules::default();
        let models = vec![ModelInfo::new("alpha-free")];
        let picked = rules.select(&models, Some("nonexistent")).expect("应回落");
        assert_eq!(picked.id, "alpha-free");
    }

    #[test]
    fn 没有免费模型时选择返回空() {
        let rules = FreeModelRules::default();
        let models = vec![ModelInfo::new("gpt-4o"), ModelInfo::new("claude-opus-4")];
        assert!(rules.select(&models, None).is_none());
    }
}
