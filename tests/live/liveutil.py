"""实模型演练层 · 公共助手。

真实模型与 mock 的根本差别:**输出不确定,且上游会限流**。因此本层的
断言只到协议形状与运行时行为,内容类判定一律宽松;单个用例失败还会
自动重试(免费池 429 / 慢首字是常态)。
"""

from __future__ import annotations

import os
import time
from typing import Any

import pytest

# 单个模型的默认重试次数(免费池 429 / 断流是常态)。
RETRIES = int(os.environ.get("REFLECT_LIVE_RETRIES", "2"))
# 重试间隔(秒)—— 退避一点,别把上游再逼限流。
RETRY_DELAY = float(os.environ.get("REFLECT_LIVE_RETRY_DELAY", "5"))


class LiveFailure(AssertionError):
    """实模型层的断言失败(带上下文)。"""


class RateLimited(Exception):
    """本轮被上游限流(429)—— **环境状态,不是产品缺陷**。

    免费池配额是全局的,429 是常态。命中它时应**跳过**用例并如实上报,
    而不是判失败 —— 否则测的是配额,不是代码。
    """

def _close(session) -> None:
    """尽力关闭一个 serve 会话。"""
    if session is None:
        return
    try:
        session.close(timeout=5.0)
    except Exception:  # noqa: BLE001 - 收尾尽力而为
        pass


def _is_rate_limited(events: list[dict[str, Any]]) -> bool:
    """判断本轮是否因限流而终止。"""
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") not in ("error", "stream_error"):
            continue
        code = str(msg.get("code", ""))
        if "RATE_LIMITED" in code.upper():
            return True
        for tried in msg.get("tried") or []:
            if "rate_limited" in str(tried.get("outcome", "")):
                return True
    return False


def submit_with_retry(
    factory,
    op: dict[str, Any],
    retries: int = RETRIES,
    delay: float = RETRY_DELAY,
) -> list[dict[str, Any]]:
    """提交一条 submission,失败时**换新会话**重试。

    `factory` 是无参可调用对象,返回一个新的 `ServeSession`。

    ⚠️ 为什么限流必须**换新会话**而不是同会话重试:429 会让凭据进入
    60s 冷却,而冷却表是**进程内**的 —— 同一进程内重试必然撞上冷却,
    直接得到 `ALL_CREDENTIALS_EXHAUSTED(0 tried)`。换新会话 = 新进程 =
    新冷却表,才谈得上「再试一次」。

    三类结局:
    - 正常收尾 → 返回事件流;
    - **限流终止** → 换新会话重试;重试用尽仍限流 → **跳过**(不算失败);
    - 其他错误 / 事件流缺终态 → 换新会话重试;用尽仍失败 → 抛断言错误。
    """
    session = None
    last_error: Exception | None = None
    try:
        for attempt in range(retries + 1):
            if session is None:
                session = factory()
            try:
                events = session.submit(op)
            except AssertionError as exc:
                # 事件流没收到终态 —— 上游中断/超时,可重试。
                last_error = exc
                if attempt >= retries:
                    raise
                _close(session)
                session = None
                time.sleep(delay)
                continue
            types = [e.get("msg", {}).get("type", "") for e in events]
            if "turn_complete" in types:
                return events
            if "error" in types:
                if _is_rate_limited(events):
                    if attempt >= retries:
                        # 限流 = 环境状态 → 跳过,不算失败。
                        pytest.skip(
                            "上游限流(429),重试用尽 —— 免费池配额是全局的,稍后重跑"
                        )
                    _close(session)
                    session = None
                    time.sleep(delay)
                    continue
                # 真实错误 —— 不重试(重试不会让一个真实缺陷变成通过)。
                return events
            last_error = LiveFailure(f"事件流缺终态:{types}")
            if attempt >= retries:
                raise last_error
            _close(session)
            session = None
            time.sleep(delay)
        raise last_error or LiveFailure("submit_with_retry 未产出事件")
    finally:
        _close(session)


def turn_status(events: list[dict[str, Any]]) -> str:
    """取本轮的终态 status(没有则返回空串)。"""
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") == "turn_complete":
            return msg.get("status", "")
    return ""


def msg_types(events: list[dict[str, Any]]) -> list[str]:
    """事件流的消息类型序列。"""
    return [e.get("msg", {}).get("type", "") for e in events]


def first_event(events: list[dict[str, Any]], msg_type: str) -> dict[str, Any] | None:
    """取第一个指定类型事件。"""
    for ev in events:
        if ev.get("msg", {}).get("type") == msg_type:
            return ev
    return None


def all_events(events: list[dict[str, Any]], msg_type: str) -> list[dict[str, Any]]:
    """取全部指定类型事件。"""
    return [e for e in events if e.get("msg", {}).get("type") == msg_type]


def text_of(events: list[dict[str, Any]]) -> str:
    """拼接本轮的全部正文增量(thinking 不计)。"""
    return "".join(
        e.get("msg", {}).get("delta", "") for e in all_events(events, "agent_message_delta")
    )


def assert_turn_shape(events: list[dict[str, Any]]) -> None:
    """断言一轮的**协议形状**(与具体内容无关)。

    这是实模型层的核心断言口径:
    - `turn_started` 起、`turn_complete` 收;
    - `turn_id` 首尾一致;
    - `token_count` 带 usage 四字段;
    - 工具调用成对(`tool_call_begin` / `tool_call_end` 数量相等)。
    """
    types = msg_types(events)
    assert types, "事件流为空"
    # `submission_closed` 是 serve 在 per-turn 通道排空后发的收尾标记,
    # 排在 `turn_complete` 之后 —— 断言终态前先滤掉它(与能力覆盖层 D2 同口径)。
    turn_types = [t for t in types if t != "submission_closed"]
    assert turn_types, "事件流里没有任何 turn 事件"
    assert turn_types[0] == "turn_started", f"事件流应以 turn_started 起:{types}"
    assert turn_types[-1] == "turn_complete", f"事件流应以 turn_complete 收:{types}"

    started = first_event(events, "turn_started")
    completed = first_event(events, "turn_complete")
    assert started is not None and completed is not None
    assert started["msg"].get("turn_id"), "turn_started 缺 turn_id"
    assert (
        completed["msg"].get("turn_id") == started["msg"].get("turn_id")
    ), "turn_complete 的 turn_id 应与 turn_started 一致"

    usage = completed["msg"].get("usage")
    assert isinstance(usage, dict), "turn_complete 缺 usage"
    for field in ("input_tokens", "output_tokens", "cached_tokens", "total_tokens"):
        assert field in usage, f"usage 缺字段 {field}:{usage}"

    begins = len(all_events(events, "tool_call_begin"))
    ends = len(all_events(events, "tool_call_end"))
    assert begins == ends, f"工具调用未成对:begin={begins} end={ends}"


def assert_no_error(events: list[dict[str, Any]]) -> None:
    """断言本轮没有 error / stream_error 事件。"""
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") in ("error", "stream_error"):
            raise LiveFailure(
                f"本轮出现 {msg.get('type')}:{msg.get('code')} {msg.get('message')}"
            )
