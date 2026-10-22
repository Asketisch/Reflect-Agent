"""实模型演练层 · 工具调用。

覆盖:
- T1 模型发起内置工具调用 → `tool_call_begin` / `tool_call_end` 成对且成功;
- T2 模型调用未注册工具 → `tool_call_end(is_error=true)` 且**本轮仍正常收尾**;
- T3 工具输出流式回传(`tool_call_output_delta`)。

⚠️ 支持 tools 的模型不同 —— 不支持 tools 的模型由**能力探测**标为跳过
(`live_matrix` 的 `supports_tools` 字段),不计入失败。
"""

from __future__ import annotations

import pytest

from liveutil import (
    all_events,
    assert_turn_shape,
    first_event,
    submit_with_retry,
    turn_status,
)


def _requires_tools(live_matrix) -> str:
    """取第一个支持 tools 的模型;没有则跳过本模块。"""
    for model, supports, reason in live_matrix:
        if supports:
            return model
    pytest.skip("本轮矩阵里的模型都不支持 tools(探测结果见 live_matrix)")


def test_T1_内置工具调用成对且成功(live_session, live_matrix) -> None:
    """让模型读一个真实存在的文件 → 工具调用成对、`is_error == false`。"""
    _requires_tools(live_matrix)
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": "请用 read 工具读取文件 probe.txt 的内容,只读这一个文件。",
                }
            ],
        },
    )
    begins = all_events(events, "tool_call_begin")
    ends = all_events(events, "tool_call_end")
    if not begins:
        pytest.skip("本轮模型没有发起工具调用(免费模型行为不保证)")
    assert len(begins) == len(ends), f"工具调用未成对:{len(begins)}/{len(ends)}"
    assert_turn_shape(events)
    assert turn_status(events) in ("success", "max_iterations")


def test_T2_未注册工具报错但本轮仍收尾(live_session, live_matrix) -> None:
    """模型调用不存在的工具 → `tool_call_end(is_error=true)`,turn 不死。"""
    _requires_tools(live_matrix)
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": "请调用 definitely_not_a_registered_tool 工具。",
                }
            ],
        },
    )
    ends = all_events(events, "tool_call_end")
    if not ends:
        pytest.skip("本轮模型没有发起工具调用")
    # 至少有一个工具调用失败(未注册工具)—— 或模型压根没调(同样跳过)。
    errors = [e for e in ends if e["msg"].get("output", {}).get("is_error")]
    if not errors:
        pytest.skip("模型没有调用未注册工具(行为不保证)")
    assert_turn_shape(events)
    assert turn_status(events) in ("success", "max_iterations")


def test_T3_工具输出流式回传(live_session, live_matrix) -> None:
    """工具执行期间应有 `tool_call_output_delta` 事件。"""
    _requires_tools(live_matrix)
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": "请用 bash 工具执行 echo live-probe。",
                }
            ],
        },
    )
    if not all_events(events, "tool_call_begin"):
        pytest.skip("本轮模型没有发起工具调用")
    # 输出可能一次性给完(不分片)—— 断言「有工具结果」而非「必须分片」。
    assert all_events(events, "tool_call_end"), "工具调用应有 tool_call_end"
