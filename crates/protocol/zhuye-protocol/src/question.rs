//! v1.1.0 P1 #14:AskUserQuestion / ask_user 协议类型。
//!
//! LLM 主动通过工具向用户发起结构化询问:多题、多选项、可选
//! `multi_select`、每题可填 "Other" 自定义文本。`AskUserQuestion` 是
//! "被动等用户响应"的事件族(与 `ApprovalRequest` 平级),由 `ApprovalGate`
//! 负责 emit + oneshot wait + 路由 `AskUserQuestionResponse` 回执。
//!
//! ## 设计取舍
//!
//! - **不复用 `ReviewDecision`**:三态(Approve / Deny / ApproveForSession)
//!   无法承载"4 选 1 + Other"的结构化答案,新协议类型必备。
//! - **`header` 长度上限 12 字符**:chip 标签规范,
//!   避免 TUI 渲染时换行错位。
//! - **`options.len()` 2-4**:选项数量 2-4;少于 2 没有"选项"意义,
//!   多于 4 难以在 modal 中浏览。
//! - **`multi_select`**:false 时每题答案唯一;true 时答案可选多个。
//! - **`custom` 字段**:每个 option 之外允许 "Other" 自定义
//!   文本;本项目 `Answer.custom: Option<String>` 表达,即使所有 option
//!   都没选也允许只填自定义文本。
//!
//! ## Wire 兼容性
//!
//! 所有新类型都是 additive addition,不影响 v1.0 消费者。

use serde::{Deserialize, Serialize};

/// 单个选项的最大字符数(用于 TUI 渲染前的截断)。
pub const MAX_HEADER_CHARS: usize = 12;

/// 一组问题的最大数量(由 config `[tool.ask_user_question].max_questions` 控制,默认 4)。
pub const MAX_QUESTIONS: usize = 4;

/// 单题选项的最小/最大数量。
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 4;

/// 一道结构化问题:头部 chip + 题目文字 + 候选选项 + 是否多选。
///
/// 典型 JSON wire 格式:
/// ```json
/// {
///   "header": "Language",       # 简短标签(≤12 字符)
///   "question": "Which language should we use for the new service?",  # 完整问题
///   "options": [                # 候选选项(2-4 个)
///     {"label": "Rust", "description": "Memory safe, fast, single binary"},
///     {"label": "Go", "description": "Simple, good for services"},
///     {"label": "TypeScript", "description": "Familiar, web integration"},
///     {"label": "Other", "description": "Specify a custom choice"}   # "Other" 允许自定义
///   ],
///   "multi_select": false       # 是否多选
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Question {
    /// 简短标签,≤12 字符,TUI 用作 chip 标签。
    pub header: String,
    /// 完整问题文字。
    pub question: String,
    /// 候选选项,2-4 个。
    pub options: Vec<QuestionOption>,
    /// `true`:用户可选 0..=N 个;`false`:必须选 1 个(或多选时≥1)。
    pub multi_select: bool,
}

impl Question {
    /// 构造一个新问题,带选项数量校验。
    ///
    /// # 错误
    ///
    /// - `header` 长度:超过 12 字符(`header.chars().count() > 12`)           # 头部过长
    /// - `options` 数量:不在 2-4 之间(`options.len() < 2 || options.len() > 4`)  # 选项数错
    pub fn new(
        header: impl Into<String>,
        question: impl Into<String>,
        options: Vec<QuestionOption>,
        multi_select: bool,
    ) -> Result<Self, QuestionError> {
        let header = header.into();
        let question = question.into();
        if header.chars().count() > MAX_HEADER_CHARS {
            return Err(QuestionError::HeaderTooLong {
                len: header.chars().count(),
                max: MAX_HEADER_CHARS,
            });
        }
        if options.len() < MIN_OPTIONS {
            return Err(QuestionError::TooFewOptions {
                got: options.len(),
                min: MIN_OPTIONS,
            });
        }
        if options.len() > MAX_OPTIONS {
            return Err(QuestionError::TooManyOptions {
                got: options.len(),
                max: MAX_OPTIONS,
            });
        }
        Ok(Self {
            header,
            question,
            options,
            multi_select,
        })
    }
}

/// 单个候选选项:标签 + 描述 + 可选预览。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionOption {
    /// 简短标签(在 TUI 中作为可选项文字,1-5 词)。
    pub label: String,
    /// 详细描述(展示在标签下方,1-2 句话)。
    pub description: String,
    /// 可选的预览内容(如代码示例,markdown 文本等)。`None` 表示无预览。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// 单题答案:`selected` 是选中的 option 下标,`custom` 是 "Other" 自定义文本。
///
/// - `multi_select=false`:`selected.len() == 1`,`custom` 可选。
/// - `multi_select=true`:`selected.len() >= 1`,`custom` 可选。
/// - 用户取消时:`selected.is_empty() && custom.is_none()`(整个 `AskUserAnswer.answers` 用空 `Answer` 填充)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Answer {
    /// 选中的 option 在 `Question.options` 里的下标列表(空 = 未选)。
    #[serde(default)]
    pub selected: Vec<usize>,
    /// "Other" 自定义文本(可选,即使 `selected` 非空也可填)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

impl Answer {
    /// 用户取消:无选项、无自定义。
    pub fn cancelled() -> Self {
        Self::default()
    }

    /// 单选(非 multi_select)。
    pub fn single(index: usize) -> Self {
        Self {
            selected: vec![index],
            custom: None,
        }
    }

    /// 多选。
    pub fn multi(indices: Vec<usize>) -> Self {
        Self {
            selected: indices,
            custom: None,
        }
    }

    /// "Other" 自定义文本(可与 selected 共存)。
    pub fn with_custom(mut self, custom: impl Into<String>) -> Self {
        self.custom = Some(custom.into());
        self
    }
}

/// 多题答案集合,`answers.len() == AskUserQuestionEvent.questions.len()`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserAnswer {
    pub answers: Vec<Answer>,
}

impl AskUserAnswer {
    /// 空答案(用户取消 / 超时,所有题都未答)。
    pub fn empty(n_questions: usize) -> Self {
        Self {
            answers: vec![Answer::cancelled(); n_questions],
        }
    }
}

/// 协议事件:LLM 主动询问用户一组问题。
///
/// 由 `AskUserQuestionTool::execute` 调 `ApprovalGate::ask_question` 时 emit;
/// TUI 收到后弹 modal,用户按键 → 回执 `Op::AskUserQuestionResponse { id, answers }`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserQuestionEvent {
    /// 配对 id:与 `Op::AskUserQuestionResponse.id` 一致。
    pub request_id: String,
    /// 问题列表(1-4 个,具体由 config `[tool.ask_user_question].max_questions` 限定)。
    pub questions: Vec<Question>,
}

impl AskUserQuestionEvent {
    pub fn new(request_id: impl Into<String>, questions: Vec<Question>) -> Self {
        Self {
            request_id: request_id.into(),
            questions,
        }
    }
}

/// 问题构造或校验失败的错误。
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum QuestionError {
    #[error("header too long: {len} chars (max {max})")]
    HeaderTooLong { len: usize, max: usize },
    #[error("too few options: {got} (min {min})")]
    TooFewOptions { got: usize, min: usize },
    #[error("too many options: {got} (max {max})")]
    TooManyOptions { got: usize, max: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(label: &str) -> QuestionOption {
        QuestionOption {
            label: label.into(),
            description: format!("{label} description"),
            preview: None,
        }
    }

    #[test]
    fn question_new_validates_header_length() {
        let opts = vec![opt("A"), opt("B")];
        let q = Question::new("Language", "Which?", opts, false);
        assert!(q.is_ok());

        let opts2 = vec![opt("A"), opt("B")];
        let long_header = "a".repeat(MAX_HEADER_CHARS + 1);
        let q2 = Question::new(long_header, "Q", opts2, false);
        assert!(matches!(q2, Err(QuestionError::HeaderTooLong { .. })));
    }

    #[test]
    fn question_new_validates_option_count() {
        // < 2
        let opts = vec![opt("A")];
        let q = Question::new("H", "Q", opts, false);
        assert!(matches!(q, Err(QuestionError::TooFewOptions { .. })));

        // > 4
        let opts = vec![opt("A"), opt("B"), opt("C"), opt("D"), opt("E")];
        let q = Question::new("H", "Q", opts, false);
        assert!(matches!(q, Err(QuestionError::TooManyOptions { .. })));

        // 边界 2 / 4 都 OK
        let opts2 = vec![opt("A"), opt("B")];
        assert!(Question::new("H", "Q", opts2, false).is_ok());
        let opts4 = vec![opt("A"), opt("B"), opt("C"), opt("D")];
        assert!(Question::new("H", "Q", opts4, true).is_ok());
    }

    #[test]
    fn ask_user_question_event_serde_roundtrip() {
        let opts = vec![opt("Rust"), opt("Go")];
        let q = Question::new("Lang", "Pick one", opts, false).unwrap();
        let ev = AskUserQuestionEvent::new("req-1", vec![q.clone()]);
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""request_id":"req-1""#));
        assert!(j.contains(r#""header":"Lang""#));
        let back: AskUserQuestionEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn ask_user_answer_empty_for_n_questions() {
        let a = AskUserAnswer::empty(3);
        assert_eq!(a.answers.len(), 3);
        for ans in &a.answers {
            assert!(ans.selected.is_empty());
            assert!(ans.custom.is_none());
        }
    }

    #[test]
    fn answer_single_multi_with_custom() {
        let a = Answer::single(0);
        assert_eq!(a.selected, vec![0]);
        assert!(a.custom.is_none());

        let a = Answer::multi(vec![0, 2]).with_custom("Mixed stack");
        assert_eq!(a.selected, vec![0, 2]);
        assert_eq!(a.custom.as_deref(), Some("Mixed stack"));
    }

    #[test]
    fn question_option_preview_omitted_when_none() {
        let o = QuestionOption {
            label: "A".into(),
            description: "B".into(),
            preview: None,
        };
        let j = serde_json::to_string(&o).unwrap();
        assert!(!j.contains("preview"), "None 应跳过:got {j}");

        let o2 = QuestionOption {
            label: "A".into(),
            description: "B".into(),
            preview: Some("```rust\nfn main() {}\n```".into()),
        };
        let j2 = serde_json::to_string(&o2).unwrap();
        assert!(j2.contains(r#""preview":"```"#));
    }
}
