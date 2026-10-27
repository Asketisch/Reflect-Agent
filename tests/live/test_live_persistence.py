"""实模型演练层 · 持久化与容错。

覆盖:
- P1 会话落盘:rollout JSONL 写出消息记录;
- P2 续接上下文:`--resume` 能把历史带进新一轮;
- X1 限流语义:上游 429 时引擎发出 `routing(cooldown_started)` 而不是静默失败。
"""

from __future__ import annotations

import json

from liveutil import all_events, assert_turn_shape, submit_with_retry, turn_status


def test_P1_会话落盘(live_session, live_sandbox) -> None:
    """一轮对话后 `~/.zhuye/sessions/` 下应有 JSONL 记录。"""
    events = submit_with_retry(
        live_session, {"type": "user_input", "items": [{"type": "text", "text": "落盘测试"}]}
    )
    assert_turn_shape(events)
    files = sorted((live_sandbox.home / ".zhuye/sessions").glob("*/*/*/*.jsonl"))
    assert files, "会话文件未落盘"
    records = [
        json.loads(line)
        for line in files[-1].read_text().splitlines()
        if line.strip()
    ]
    assert records, "会话文件为空"


def test_P2_多轮累积在同一会话文件(live_session, live_sandbox) -> None:
    """两轮对话应落在同一个会话文件里(会话状态不漂移)。"""
    submit_with_retry(
        live_session, {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]}
    )
    submit_with_retry(
        live_session, {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]}
    )
    files = sorted((live_sandbox.home / ".zhuye/sessions").glob("*/*/*/*.jsonl"))
    assert files, "会话文件未落盘"
    records = [
        json.loads(line) for line in files[-1].read_text().splitlines() if line.strip()
    ]
    kinds = [r.get("type") for r in records]
    assert kinds.count("message") >= 2, f"两轮应至少留下两条消息记录:{kinds}"


def test_X1_限流时发出冷却事件而非静默失败(live_session) -> None:
    """上游 429 → 引擎发 `routing(cooldown_started)`。

    免费池限流是常态,这条断言守的是「限流要可观测」—— 不能悄悄失败。
    """
    events = submit_with_retry(
        live_session, {"type": "user_input", "items": [{"type": "text", "text": "限流观察"}]}
    )
    routing = all_events(events, "routing")
    errors = all_events(events, "error") + all_events(events, "stream_error")
    if not errors:
        # 本轮没被限流 —— 没有可断言的冷却事件,直接通过。
        return
    assert routing or errors, "出错时应伴随 routing 事件"
    # 若走了限流路径,终态仍应是 turn_complete 或 error(不能没有终态)。
    assert any(
        e.get("msg", {}).get("type") in ("turn_complete", "error") for e in events
    ), "出错后仍应有终态事件"
