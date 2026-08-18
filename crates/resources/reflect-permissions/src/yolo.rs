//! `yolo` —— YOLO 审批建议 stub(P2 `yolo-classifier`)。
//!
//! v2 将接入 LLM 做 Allow/Deny/Ask 建议;当前用启发式规则 + 固定
//! 置信度,供 ApprovalGate 在 Auto 模式下参考。

use serde::{Deserialize, Serialize};

use crate::rules::RuleMatch;

/// LLM 审批建议(或启发式 stub 的输出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YoloSuggestion {
    Allow,
    Deny,
    Ask,
}

impl From<RuleMatch> for YoloSuggestion {
    fn from(m: RuleMatch) -> Self {
        match m {
            RuleMatch::Allow => Self::Allow,
            RuleMatch::Deny => Self::Deny,
            RuleMatch::Ask | RuleMatch::NoMatch => Self::Ask,
        }
    }
}

/// 分类器输出 —— 含建议动作与 0.0–1.0 置信度。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoloClassification {
    pub suggestion: YoloSuggestion,
    pub confidence: f32,
    pub reason: String,
}

/// YOLO 分类器 trait —— 未来可换 LLM 后端。
pub trait YoloClassifier: Send + Sync {
    fn classify(&self, tool_name: &str, tool_args: &str) -> YoloClassification;
}

/// 启发式 stub:只读工具倾向 Allow,破坏性 Bash 倾向 Ask。
#[derive(Debug, Default)]
pub struct HeuristicYoloClassifier;

impl YoloClassifier for HeuristicYoloClassifier {
    fn classify(&self, tool_name: &str, tool_args: &str) -> YoloClassification {
        let lower_tool = tool_name.to_ascii_lowercase();
        if matches!(lower_tool.as_str(), "read" | "grep" | "glob" | "echo") {
            return YoloClassification {
                suggestion: YoloSuggestion::Allow,
                confidence: 0.85,
                reason: "只读工具,启发式允许".into(),
            };
        }
        if lower_tool == "bash" {
            let lower_args = tool_args.to_ascii_lowercase();
            if lower_args.contains("rm -rf") || lower_args.contains("sudo ") {
                return YoloClassification {
                    suggestion: YoloSuggestion::Ask,
                    confidence: 0.9,
                    reason: "检测到高风险 shell 命令".into(),
                };
            }
        }
        YoloClassification {
            suggestion: YoloSuggestion::Ask,
            confidence: 0.5,
            reason: "默认需人工确认(stub)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tool_suggests_allow() {
        let c = HeuristicYoloClassifier.classify("Read", "{}");
        assert_eq!(c.suggestion, YoloSuggestion::Allow);
        assert!(c.confidence > 0.8);
    }

    #[test]
    fn destructive_bash_suggests_ask() {
        let c = HeuristicYoloClassifier.classify("Bash", r#"{"command":"rm -rf /"}"#);
        assert_eq!(c.suggestion, YoloSuggestion::Ask);
        assert!(c.confidence >= 0.9);
    }

    #[test]
    fn unknown_tool_defaults_to_ask() {
        let c = HeuristicYoloClassifier.classify("Write", "{}");
        assert_eq!(c.suggestion, YoloSuggestion::Ask);
    }
}
