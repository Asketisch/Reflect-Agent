"""能力覆盖层 · 编排面:子代理、讨论、任务/团队、流水线、goal 模式。

子代理部分走 serve wire(模型调 `call_<role>`);讨论 / 任务 / 流水线走 CLI。

覆盖:
- O1 子代理调用(内置 explorer spec → `call_explorer` 工具);
- O2 讨论编排(mock 驱动真多 agent,transcript + Result);
- O3 任务 / 团队(task create/ls/update);
- O4 流水线(预设模板受限 → 错误路径断言:no runner / team not found);
- O5 goal 模式与其他 op 共存(进出 goal 模式不破坏会话)。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_orchestration.py -v
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    first_event,
    msg_types,
    require_binary,
    run_cli,
)

pytestmark = pytest.mark.usefixtures("sandbox")


def test_O1_子代理调用(serve_session, mock_script) -> None:
    """模型调 `call_explorer` → 子代理执行 → 回执进历史,turn 正常收尾。

    依据:crates/orchestration/zhuye-subagent/src/spec.rs:31-32
    (工具名 = `call_<role>`);bootstrap.rs:286(无自定义 spec 时回退硬编码
    explorer)。
    """
    require_binary()
    # 父模型 → 调子代理;子代理自己也要调一次模型;父模型再收尾。
    # `call_<role>` 的入参是 `prompt`(子代理任务描述)。
    script = mock_script(
        [
            '{"type":"tool_call","name":"call_explorer","args":{"prompt":"探查一下"}}',
            '{"type":"text","text":"subagent-answer"}',
            '{"type":"text","text":"parent-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "派个子代理"}]},
            timeout=DRAIN_TIMEOUT,
        )
    begin = first_event(evs, "tool_call_begin")
    assert begin is not None, f"未见 tool_call_begin:{msg_types(evs)}"
    assert begin["msg"]["tool_name"] == "call_explorer", (
        f"tool_name={begin['msg']['tool_name']!r}"
    )
    end = first_event(evs, "tool_call_end")
    assert end is not None, f"未见 tool_call_end:{msg_types(evs)}"
    assert end["msg"]["is_error"] is False, f"子代理应成功:{end['msg']}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_O2_讨论编排(serve_session, mock_script, sandbox) -> None:
    """`zhuye discussion run`(mock 驱动真多 agent)→ transcript + Result。

    依据:tests/rust/cli_deep.sh 组 9。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"send_message","args":{"content":"advocate says async","kind":"utterance"}}',
            '{"type":"tool_call","name":"send_message","args":{"content":"skeptic says sync","kind":"utterance"}}',
            '{"type":"text","text":"a"}',
            '{"type":"text","text":"b"}',
        ]
    )
    cfg = sandbox.cwd / "disc.toml"
    cfg.write_text(
        "[discussion]\n"
        'mode = "sequential"\n'
        'topic = "deep-topic"\n'
        "max_rounds = 1\n\n"
        "[[agents]]\n"
        'role = "advocate"\n'
        'system_prompt = "advocate"\n'
        'allowed_tools = ["send_message", "read_messages", "finish_discussion"]\n\n'
        "[[agents]]\n"
        'role = "skeptic"\n'
        'system_prompt = "skeptic"\n'
        'allowed_tools = ["send_message", "read_messages", "finish_discussion"]\n'
    )
    proc = run_cli(
        sandbox,
        "discussion",
        "run",
        "-c",
        str(cfg),
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)},
        timeout=90,
    )
    assert proc.returncode == 0, f"discussion run 退出码={proc.returncode}:{proc.stderr}"
    assert "=== Discussion Transcript ===" in proc.stdout, f"缺 transcript 段:{proc.stdout[:500]}"
    assert "=== Result ===" in proc.stdout, f"缺 Result 段:{proc.stdout[:500]}"
    assert "rounds_completed" in proc.stdout, f"缺 rounds_completed:{proc.stdout[:500]}"
    assert "advocate says async" in proc.stdout, f"transcript 缺 advocate 发言"
    assert "skeptic says sync" in proc.stdout, f"transcript 缺 skeptic 发言"


def test_O3_任务与团队(sandbox) -> None:
    """`zhuye task create/ls/update` → 任务落盘且可列出。"""
    require_binary()
    created = run_cli(
        sandbox,
        "task",
        "create",
        "--subject",
        "S1",
        "--list",
        "L1",
        "--description",
        "d1",
        "--active-form",
        "Doing",
        env={"ZHUYE_MODEL": "mock"},
        timeout=30,
    )
    assert created.returncode == 0, f"task create 退出码={created.returncode}:{created.stderr}"
    assert "Task #1 created" in created.stdout, f"回显异常:{created.stdout!r}"
    listed = run_cli(
        sandbox, "task", "ls", env={"ZHUYE_MODEL": "mock"}, timeout=30
    )
    assert listed.returncode == 0, f"task ls 退出码={listed.returncode}:{listed.stderr}"
    assert "S1" in listed.stdout, f"task ls 未列出:{listed.stdout!r}"


def test_O4_流水线_错误路径(sandbox) -> None:
    """pipeline:预设模板受限 → 错误路径断言(no runner / team not found)。

    依据:tests/rust/cli_deep.sh 组 9;产品限制见 tests/COVERAGE.md §5。
    """
    require_binary()
    ghost = run_cli(
        sandbox,
        "pipeline",
        "team",
        "ghost",
        "--topic",
        "t",
        env={"ZHUYE_MODEL": "mock"},
        timeout=30,
    )
    assert ghost.returncode != 0, "pipeline team 不存在应报错"
    assert "team not found" in (ghost.stdout + ghost.stderr), (
        f"报错文案异常:{ghost.stdout + ghost.stderr!r}"
    )

    team = run_cli(
        sandbox, "task", "team", "create", "only", env={"ZHUYE_MODEL": "mock"}, timeout=30
    )
    assert team.returncode == 0, f"task team create 退出码={team.returncode}:{team.stderr}"
    cfg = sandbox.cwd / "pipe.toml"
    cfg.write_text(
        '[pipeline]\nname = "deep"\nfailure_policy = "abort"\n\n'
        '[nodes.only]\nteam = "only"\ntemplate = "Do: {{topic}}"\ndepends_on = []\n'
    )
    run = run_cli(
        sandbox,
        "pipeline",
        "run",
        "-c",
        str(cfg),
        "--topic",
        "t1",
        env={"ZHUYE_MODEL": "mock"},
        timeout=60,
    )
    assert run.returncode != 0, "自定义 label 无 runner 应报错"
    assert "no runner" in (run.stdout + run.stderr), (
        f"报错文案异常:{run.stdout + run.stderr!r}"
    )


def test_O5_goal模式与其他op共存(serve_session, mock_script) -> None:
    """goal 模式下其他 op 仍可用,进出 goal 模式不破坏会话。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"g1"}',
            '{"type":"text","text":"g2"}',
            '{"type":"text","text":"g3"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {
                "type": "enter_goal_mode",
                "goal": "probe goal",
                "token_budget": 100000,
            },
            timeout=DRAIN_TIMEOUT,
        )
        evs = s.submit({"type": "compact"}, timeout=40.0)
        assert "context_compacted" in msg_types(evs), f"goal 模式下 compact 失效:{msg_types(evs)}"
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "干活"}]},
            timeout=DRAIN_TIMEOUT,
        )
        assert "turn_complete" in msg_types(evs), f"goal 模式下 turn 未完成:{msg_types(evs)}"
        s.submit({"type": "exit_goal_mode"}, timeout=DRAIN_TIMEOUT)
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "再干活"}]},
            timeout=DRAIN_TIMEOUT,
        )
    assert "turn_complete" in msg_types(evs), f"退出 goal 后 turn 未完成:{msg_types(evs)}"
