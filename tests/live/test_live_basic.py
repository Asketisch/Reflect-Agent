"""实模型演练层 · 基础通信与事件流。

覆盖(协议形状级断言,不断言具体回答文本):
- L1 握手:`session_configured` 字段完整;
- L2 单轮事件序列:`turn_started` → delta → `token_count` → `turn_complete`;
- L3 usage 字段齐全且非零(结束原因之后的尾部 usage 必须被采集);
- L4 终态语义:`status == "success"`;
- L5 多轮上下文:第二轮仍在同一 session 内正常收尾。
"""

from __future__ import annotations

from liveutil import (
    assert_no_error,
    assert_turn_shape,
    first_event,
    submit_with_retry,
    text_of,
    turn_status,
)


def test_L1_握手事件字段完整(live_session) -> None:
    """serve 启动即 emit 全局 `session_configured`(id=="")。"""
    with live_session() as s:
        ev = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "session_configured",
            timeout=60,
        )
        assert ev["id"] == "", "握手事件应是全局事件(id 为空)"
        m = ev["msg"]
        assert m["provider"], "session_configured 缺 provider"
        assert m["model"], "session_configured 缺 model"
        assert "/" in m["model"], f"model 应是完整 spec: {m.get('model')!r}"
        assert m["session_id"], "session_id 不应为空"


def test_L2_单轮事件序列与终态(live_session) -> None:
    """一轮的完整事件流:形状正确 + 状态为 success。"""
    events = submit_with_retry(
        live_session,
        {"type": "user_input", "items": [{"type": "text", "text": "说一句话"}]},
    )
    assert_no_error(events)
    assert_turn_shape(events)
    assert turn_status(events) == "success", f"turn 状态:{turn_status(events)!r}"
    assert text_of(events), "本轮应有正文输出"


def test_L3_usage字段齐全且非零(live_session) -> None:
    """`turn_complete.usage` 四字段齐全,且 `total_tokens > 0`。

    这条断言守着「尾部 usage chunk 丢失」的回归:某些 provider 把 usage
    放在结束原因**之后**的额外 chunk 里,漏收会让 token_count 恒为 0。
    """
    events = submit_with_retry(
        live_session, {"type": "user_input", "items": [{"type": "text", "text": "hi"}]}
    )
    assert_no_error(events)
    completed = first_event(events, "turn_complete")
    assert completed is not None
    usage = completed["msg"]["usage"]
    assert usage["total_tokens"] > 0, f"usage 应非零:{usage}"
    assert usage["input_tokens"] > 0, f"input_tokens 应非零:{usage}"


def test_L4_多轮上下文(live_session) -> None:
    """同一 session 内连续两轮都正常收尾。

    本用例**刻意用同一个会话**发两轮(多轮上下文的本意)—— 因此不能用
    「换新会话重试」的 `submit_with_retry`:429 会让凭据进 60s 冷却,同会话
    内第二轮必然撞冷却。这里改为:任一轮被限流 → 跳过。
    """
    import pytest

    from liveutil import _is_rate_limited

    with live_session() as s:
        first = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]}
        )
        if _is_rate_limited(first):
            pytest.skip("上游限流(429)—— 跳过")
        second = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]}
        )
    if _is_rate_limited(second):
        pytest.skip("上游限流(429)—— 跳过")
    assert_no_error(first)
    assert_no_error(second)
    assert_turn_shape(second)
    assert turn_status(second) == "success"
