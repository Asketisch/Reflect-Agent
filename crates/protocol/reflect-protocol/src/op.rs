//! Op —— 客户端可向 core 提交的操作。

use serde::{Deserialize, Serialize};

use crate::item::{
    PlanApprovalChoice, ReasoningEffortMirror, ReviewDecision, ThreadSettingsOverrides,
    UserInputItem,
};
use crate::question::AskUserAnswer;

/// 用户或系统发起的操作。
///
/// v0 有 6 个 variant;v1.x 加 `EnterPlanMode` / `ExitPlanMode` 两个
/// 共 8 个;v1.x S4 加 `SetEffort` 共 9 个。按协议 §7,新增 variant
/// 是 non-breaking addition。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Op {
    /// 用户文本/图像输入,驱动新一轮对话。
    UserInput {
        items: Vec<UserInputItem>,
        #[serde(default)]
        thread_settings: ThreadSettingsOverrides,
    },

    /// 手动触发上下文压缩。
    Compact,

    /// 中断当前 turn(保留状态,丢弃进行中的流)。
    ///
    /// B3:`child_id = Some(...)` 请求中断特定子 agent(引擎端需
    /// SubagentRuntimeRegistry 支持);`None`(默认) = 中断整个会话(既有行为)。
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_id: Option<String>,
    },

    /// 批次十九:回退对话到指定节点(原地回滚)。`to_turn_id = None` 表示回退
    /// 到最近一条 user turn(最常用的「编辑并重发上一条」)。submission_loop
    /// 收到后:**截断 rollout 记录器**(删除该 turn 之后的所有记录,持久化),
    /// 并 emit `TurnRewound` 事件让 TUI 同步裁剪显示。
    Rewind {
        #[serde(default)]
        to_turn_id: Option<String>,
    },

    /// 优雅关闭线程。
    Shutdown,

    /// 响应之前发出的 ToolApprovalRequest。
    ToolApproval {
        id: String,
        decision: ReviewDecision,
    },

    /// 响应之前发出的 HookApprovalRequest。
    HookApproval {
        id: String,
        decision: ReviewDecision,
    },

    /// v1.x Plan mode:请求进入只读规划阶段。
    ///
    /// 注意这是**请求**而非立即切换——submission loop 收到后
    /// 会 emit `EventMsg::PlanRequest { task }`,等用户在 TUI 弹窗确认
    /// 后才真正把 `PermissionMode` 切到 `Plan`。
    EnterPlanMode { task: String },

    /// v1.x Plan mode:请求退出规划阶段。
    ///
    /// 由 `ExitPlanModeTool` 或 `/exit-plan` slash 触发;submission loop
    /// 会 emit `EventMsg::PlanReady { plan_id, markdown }` 给用户审批,
    /// 审批通过后 `PermissionMode` 切回 `Prompt`,写工具解锁。
    ExitPlanMode,

    /// v1.x Plan mode:TUI 在 plan approval modal 上按 Y/N/A 后回执,
    /// 投递用户决策到对应 `PlanId` 的 waiter。
    ///
    /// `id` 是 `PlanId` 的字符串形式(与 `PlanRequestEvent.task` /
    /// `PlanReadyEvent.plan_id` 一致),由 TUI 从 emit 的 event 中取出后
    /// 回填。
    PlanApproval {
        id: String,
        choice: PlanApprovalChoice,
    },

    /// v1.x S4:切换思考 / reasoning 深度。`submission_loop` 收到后写入
    /// `AgentConfig.effort` 槽,下一轮 `model_call` 构造 `ChatRequest`
    /// 时按当前 effort 选 `ThinkingConfig::OpenAIReasoning { effort }`。
    ///
    /// **设计取舍**:不放在 `Op::UserInput.thread_settings.effort`,因为
    /// `/effort low` slash 不需要等下一条 user prompt 就能切(用户跑了
    /// 5 轮后想调高推理深度,不必再输一句 prompt)。独立 Op 让
    /// `submission_loop` 单点处理 + 立即写入 slot。
    SetEffort { effort: ReasoningEffortMirror },

    /// v1.1.0 P1 #14:用户对 LLM 主动询问(`EventMsg::AskUserQuestion`)的
    /// 结构化回答。`id` 与 `AskUserQuestionEvent.request_id` 配对;
    /// `answers.answers.len() == event.questions.len()`,每题一个 `Answer`
    /// 槽。用户按 Esc 取消时 `answers.answers` 用空 `Answer` 填充,让
    /// LLM 知道"用户没回答"。
    AskUserQuestionResponse { id: String, answers: AskUserAnswer },

    /// v1.1.0 P1 #15:用户对 `ask_user` 自由文本询问的回执。`id` 与
    /// `AskUserInputEvent.request_id` 配对;`text` 为用户输入(空字符串
    /// 表示 Esc 取消或未填)。
    AskUserInputResponse { id: String, text: String },

    /// v1.1.0 P1:切换会话级 `PermissionMode`(TUI `/mode` / Shift+Tab)。
    /// `submission_loop` 写入 `AgentConfig.permission_mode` 并 emit
    /// `EventMsg::PermissionModeChanged`。
    SetPermissionMode { mode: crate::item::PermissionMode },

    /// v1.1.0 P1:循环切换 `PermissionMode`(TUI 快捷操作)。等价于
    /// `SetPermissionMode { mode: current.next_in_ui_cycle() }`。
    CyclePermissionMode,

    /// v1.2 P1:目标模式。设定目标 + 可选命令
    /// 校验 + token 预算。submission_loop 收到后构造 `GoalController` 放入
    /// `NodeContext.goal`,后续每轮 turn 结束由 controller 自校验(LM judge)
    /// + 可选命令,未完成则推 steering 续作,完成则退出。
    EnterGoalMode {
        goal: String,
        #[serde(default)]
        verify_command: Option<String>,
        #[serde(default)]
        token_budget: Option<u64>,
    },

    /// v1.2 P1:退出目标模式(`/goal clear`)。submission_loop 收到后清空
    /// `NodeContext.goal`,停止自动续作。
    ExitGoalMode,
}

impl Op {
    /// 单文本 UserInput 的便捷构造器。
    pub fn user_input_text(text: impl Into<String>) -> Self {
        Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: ThreadSettingsOverrides::default(),
        }
    }

    /// 稳定的字符串判别器(用于测试和日志)。
    pub fn discriminant(&self) -> &'static str {
        match self {
            Op::UserInput { .. } => "user_input",
            Op::Compact => "compact",
            Op::Interrupt { .. } => "interrupt",
            Op::Rewind { .. } => "rewind",
            Op::Shutdown => "shutdown",
            Op::ToolApproval { .. } => "tool_approval",
            Op::HookApproval { .. } => "hook_approval",
            Op::EnterPlanMode { .. } => "enter_plan_mode",
            Op::ExitPlanMode => "exit_plan_mode",
            Op::PlanApproval { .. } => "plan_approval",
            Op::SetEffort { .. } => "set_effort",
            Op::AskUserQuestionResponse { .. } => "ask_user_question_response",
            Op::AskUserInputResponse { .. } => "ask_user_input_response",
            Op::SetPermissionMode { .. } => "set_permission_mode",
            Op::CyclePermissionMode => "cycle_permission_mode",
            Op::EnterGoalMode { .. } => "enter_goal_mode",
            Op::ExitGoalMode => "exit_goal_mode",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{ReviewDecision, ThreadSettingsOverrides, UserInputItem};

    #[test]
    fn discriminant_is_stable() {
        assert_eq!(Op::Compact.discriminant(), "compact");
        assert_eq!(Op::Interrupt { child_id: None }.discriminant(), "interrupt");
        assert_eq!(Op::Shutdown.discriminant(), "shutdown");
        // 批次十九:Op::Rewind discriminant。
        assert_eq!(Op::Rewind { to_turn_id: None }.discriminant(), "rewind");
        assert_eq!(
            Op::Rewind {
                to_turn_id: Some("turn-42".into())
            }
            .discriminant(),
            "rewind"
        );
    }

    #[test]
    fn rewind_serde_roundtrip() {
        let op = Op::Rewind {
            to_turn_id: Some("turn-7".into()),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, Op::Rewind { ref to_turn_id } if to_turn_id.as_deref() == Some("turn-7"))
        );
    }

    #[test]
    fn rewind_serde_default_to_turn_id() {
        // `#[serde(default)]` 让缺 to_turn_id 的 JSON 反序列化为 None。
        // Op 是默认 externally-tagged serde,故 Rewind 的 JSON 是
        // {"Rewind":{"to_turn_id":null}} 或省略 → None。
        let op = Op::Rewind { to_turn_id: None };
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Op::Rewind { to_turn_id: None }));
    }

    #[test]
    fn serde_roundtrip_user_input() {
        let op = Op::UserInput {
            items: vec![UserInputItem::Text { text: "hi".into() }],
            thread_settings: ThreadSettingsOverrides::default(),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"user_input""#));
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(back.discriminant(), "user_input");
    }

    #[test]
    fn serde_roundtrip_tool_approval() {
        let op = Op::ToolApproval {
            id: "abc".into(),
            decision: ReviewDecision::Deny {
                reason: "no".into(),
            },
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(back.discriminant(), "tool_approval");
    }

    #[test]
    fn serde_roundtrip_enter_plan_mode() {
        // v1.x: EnterPlanMode { task } 必须能被 serde 识别。
        let op = Op::EnterPlanMode {
            task: "refactor the auth module".into(),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"enter_plan_mode""#), "got: {json}");
        let back: Op = serde_json::from_str(&json).unwrap();
        match back {
            Op::EnterPlanMode { task } => assert_eq!(task, "refactor the auth module"),
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(op.discriminant(), "enter_plan_mode");
    }

    #[test]
    fn serde_roundtrip_exit_plan_mode() {
        // v1.x: ExitPlanMode 是无字段 variant。
        let op = Op::ExitPlanMode;
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"exit_plan_mode""#), "got: {json}");
        let back: Op = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Op::ExitPlanMode));
        assert_eq!(op.discriminant(), "exit_plan_mode");
    }

    #[test]
    fn serde_roundtrip_plan_approval() {
        // v1.x: Op::PlanApproval { id, choice } —— TUI modal 在 1/2/3
        // 选择后回执的载体(choice 决定批准后的目标 PermissionMode)。
        let op = Op::PlanApproval {
            id: "plan-uuid".into(),
            choice: PlanApprovalChoice::AutoMode,
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"plan_approval""#), "got: {json}");
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(back.discriminant(), "plan_approval");
    }

    #[test]
    fn serde_roundtrip_set_effort() {
        // v1.x S4:Op::SetEffort { effort } —— `/effort low|medium|high`
        // slash 命令的协议载体。所有 3 个 enum variant 都覆盖。
        for effort in [
            crate::item::ReasoningEffortMirror::Low,
            crate::item::ReasoningEffortMirror::Medium,
            crate::item::ReasoningEffortMirror::High,
        ] {
            let op = Op::SetEffort { effort };
            let json = serde_json::to_string(&op).unwrap();
            assert!(json.contains(r#""type":"set_effort""#), "got: {json}");
            let back: Op = serde_json::from_str(&json).unwrap();
            match back {
                Op::SetEffort { effort: e } => assert_eq!(e, effort),
                _ => panic!("wrong variant"),
            }
            assert_eq!(op.discriminant(), "set_effort");
        }
    }

    #[test]
    fn serde_roundtrip_ask_user_question_response() {
        // v1.1.0 P1 #14: TUI 在 question modal 上提交答案后,submission
        // loop 收到 `Op::AskUserQuestionResponse { id, answers }`,
        // ApprovalGate 据 `id` 找到 waiter,把 `answers` 发给
        // `AskUserQuestionTool::execute`。
        use crate::question::{Answer, AskUserAnswer};
        let op = Op::AskUserQuestionResponse {
            id: "q-uuid-1".into(),
            answers: AskUserAnswer {
                answers: vec![
                    Answer::single(0).with_custom("prefer async"),
                    Answer::multi(vec![1, 2]),
                ],
            },
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(
            json.contains(r#""type":"ask_user_question_response""#),
            "got: {json}"
        );
        let back: Op = serde_json::from_str(&json).unwrap();
        match back {
            Op::AskUserQuestionResponse { id, answers } => {
                assert_eq!(id, "q-uuid-1");
                assert_eq!(answers.answers.len(), 2);
                assert_eq!(answers.answers[0].selected, vec![0]);
                assert_eq!(answers.answers[0].custom.as_deref(), Some("prefer async"));
                assert_eq!(answers.answers[1].selected, vec![1, 2]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(op.discriminant(), "ask_user_question_response");
    }

    #[test]
    fn serde_roundtrip_ask_user_question_response_empty() {
        // 用户按 Esc 取消时,answers 用空 Answer 填充。
        use crate::question::AskUserAnswer;
        let op = Op::AskUserQuestionResponse {
            id: "q-cancelled".into(),
            answers: AskUserAnswer::empty(3),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        if let Op::AskUserQuestionResponse { id, answers } = back {
            assert_eq!(id, "q-cancelled");
            assert_eq!(answers.answers.len(), 3);
            for a in &answers.answers {
                assert!(a.selected.is_empty());
                assert!(a.custom.is_none());
            }
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn serde_roundtrip_enter_goal_mode() {
        // v1.2 P1:EnterGoalMode { goal, verify_command, token_budget } 来回序列化测试。
        let op = Op::EnterGoalMode {
            goal: "make all tests pass".into(),
            verify_command: Some("cargo test".into()),
            token_budget: Some(100_000),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"enter_goal_mode""#), "got: {json}");
        let back: Op = serde_json::from_str(&json).unwrap();
        match back {
            Op::EnterGoalMode {
                goal,
                verify_command,
                token_budget,
            } => {
                assert_eq!(goal, "make all tests pass");
                assert_eq!(verify_command.as_deref(), Some("cargo test"));
                assert_eq!(token_budget, Some(100_000));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(op.discriminant(), "enter_goal_mode");
    }

    #[test]
    fn serde_roundtrip_exit_goal_mode() {
        // v1.2 P1: ExitGoalMode 无字段 variant。
        let op = Op::ExitGoalMode;
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains(r#""type":"exit_goal_mode""#), "got: {json}");
        let back: Op = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Op::ExitGoalMode));
        assert_eq!(op.discriminant(), "exit_goal_mode");
    }

    #[test]
    fn enter_goal_mode_defaults_optional_fields() {
        // verify_command / token_budget 缺省 → None(serde default)。
        let j = r#"{"type":"enter_goal_mode","goal":"hi"}"#;
        let back: Op = serde_json::from_str(j).unwrap();
        match back {
            Op::EnterGoalMode {
                verify_command,
                token_budget,
                ..
            } => {
                assert_eq!(verify_command, None);
                assert_eq!(token_budget, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
