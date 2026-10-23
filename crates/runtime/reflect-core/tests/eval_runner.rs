//! v1.5 E3 — 数据驱动评估运行器测试。
//!
//! 覆盖:场景 JSON 往返、工具环场景通过、断言失败时报告携带失败原因、
//! 多回合 + Steer + Interrupt 组合场景。

use std::path::Path;

use reflect_core::{EvalExpectations, EvalOp, EvalScenario, FinalStatus, run_scenario};

fn base_json(name: &str, script: &str, expect: &str) -> String {
    format!(
        r#"{{
            "name": "{name}",
            "ops": [
                {{ "user_input": {{ "text": "run echo ping", "wait": true }} }}
            ],
            "script": {script},
            "expect": {expect}
        }}"#
    )
}

/// JSON 往返:from_json_str 解析 + serde 再序列化无损(关键字段)。
#[test]
fn scenario_json_roundtrip() {
    let json = base_json(
        "smoke",
        r#"[[
            {"text":{"text":"hi"}},
            {"usage":{"input_tokens":10,"output_tokens":2}}
        ]]"#,
        r#"{"event_order":["turn_started","turn_complete"],"final_status":"success"}"#,
    );
    let sc = EvalScenario::from_json_str(&json).unwrap();
    assert_eq!(sc.name, "smoke");
    assert_eq!(sc.ops.len(), 1);
    assert!(matches!(sc.ops[0], EvalOp::UserInput { wait: true, .. }));
    assert_eq!(sc.script.len(), 1);
    assert_eq!(sc.expect.final_status, FinalStatus::Success);

    // 再序列化应保留关键字段(ScriptStep 形态可还原)。
    let re = serde_json::to_string(&sc).unwrap();
    assert!(re.contains("text"));
    let sc2 = EvalScenario::from_json_str(&re).unwrap();
    assert_eq!(sc2.name, sc.name);
}

/// 工具环场景端到端:模型调 echo → 第二次请求含工具结果 → Success。
#[tokio::test]
async fn eval_tool_loop_scenario_passes() {
    let json = base_json(
        "tool-loop",
        r#"[[
            {"tool_use":{"id":"c1","name":"echo","args":{"text":"ping"}}},
            {"usage":{"input_tokens":100,"output_tokens":20}}
        ],
        [
            {"text":{"text":"done"}}
        ]]"#,
        r#"{
            "event_order":["turn_started","tool_call_begin","tool_call_end","turn_complete"],
            "tool_calls":["echo"],
            "request_contains":{"1":["ping"]},
            "final_status":"success",
            "model_calls":2
        }"#,
    );
    let scenario = EvalScenario::from_json_str(&json).unwrap();
    let report = run_scenario(&scenario, Path::new(".")).await;
    assert!(
        report.passed,
        "工具环场景应通过,失败: {:?}",
        report.failures
    );
    assert_eq!(report.model_calls, 2);
}

/// 断言失败:期望的工具序列与实际不符 → 报告携带失败原因且 passed=false。
#[tokio::test]
async fn eval_reports_assertion_failures() {
    let json = base_json(
        "wrong-tools",
        r#"[[]]"#,
        r#"{"tool_calls":["bash"],"model_calls":1}"#,
    );
    let scenario = EvalScenario::from_json_str(&json).unwrap();
    let report = run_scenario(&scenario, Path::new(".")).await;
    assert!(!report.passed, "断言不满足必须报失败");
    assert!(
        report.failures.iter().any(|f| f.contains("tool_calls")),
        "失败原因应含 tool_calls: {:?}",
        report.failures
    );
}

/// 多回合组合:无等待回合被 Interrupt 打断(aborted 终态)+
/// Steer 在下一回合边界合并。
#[tokio::test]
async fn eval_multi_op_scenario_with_interrupt_and_steer() {
    let scenario = EvalScenario {
        name: "interrupt-steer".into(),
        ops: vec![
            // 发起慢回合(不等待)。
            EvalOp::UserInput {
                text: "写长文".into(),
                wait: false,
            },
            EvalOp::Interrupt,
            EvalOp::Steer {
                text: "改用中文".into(),
            },
            // 收尾回合:转向合并进边界。
            EvalOp::UserInput {
                text: "继续".into(),
                wait: true,
            },
        ],
        script: vec![
            // 慢回合:脚本耗尽兜底为立即停止(会被打断,无所谓内容)。
            vec![],
            // 收尾回合:空文本。
            vec![],
        ],
        expect: EvalExpectations {
            event_order: vec![
                "turn_started".into(),
                "turn_aborted".into(),
                "turn_started".into(),
                "turn_complete".into(),
            ],
            tool_calls: vec![],
            request_contains: {
                let mut m = std::collections::HashMap::new();
                // 第二次模型请求(收尾回合)应含合并的转向文本。
                m.insert("1".to_string(), vec!["改用中文".to_string()]);
                m
            },
            final_status: FinalStatus::Success,
            model_calls: Some(2),
        },
    };
    let report = run_scenario(&scenario, Path::new(".")).await;
    assert!(
        report.passed,
        "多操作场景应通过,失败: {:?}",
        report.failures
    );
}

/// 场景文件加载(from_json_file)+ 批量运行(run_all)。
#[tokio::test]
async fn eval_from_file_and_run_all() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scenario.json");
    std::fs::write(
        &path,
        base_json(
            "from-file",
            r#"[[{"text":{"text":"ok"}}]]"#,
            r#"{"final_status":"success"}"#,
        ),
    )
    .unwrap();
    let scenario = EvalScenario::from_json_file(&path).unwrap();
    let reports = reflect_core::run_all(&[scenario], Path::new(".")).await;
    assert_eq!(reports.len(), 1);
    assert!(reports[0].passed, "{:?}", reports[0].failures);
}
