"""Python SDK 绑定层:SDK 客户端自身的行为(不测服务端语义)。

与能力覆盖层(`tests/functional/`,原始 wire 打真二进制)互补:
本文件驱动**真二进制**,但断言的是 **Python SDK 怎么反应** —— 迭代器何时
收尾、handler 怎么被调、回执怎么写、close 怎么收尾。

覆盖:
- K1 握手:spawn 返回即就绪(收到 `session_configured` 才返回);
- K2 单轮 / 多轮 prompt:文本回流、多轮脚本按序消耗;
- K3 远程工具:注册 → LLM 调用 → 本地 handler 执行 → 回执 → turn_complete;
- K4 非 turn op 收尾:`submit_op` 迭代器以 `submission_closed` 结束;
- K5 interrupt 打断:收到 `turn_aborted`,原 turn 随后 `turn_complete`;
- K6 close 幂等;
- K7 坏二进制:握手超时/EOF 时抛 `RuntimeError`;
- K8 handler 抛异常:SDK 转成 `is_error=true` 的错误回执(不让 turn 挂死)。

运行:tests/run_all.sh sdk-python,或
  ZHUYE_BIN=... PYTHONPATH=sdks/python python3 -m pytest tests/sdk-python -v
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from sdkharness import drain, make_options, msg_types

from zhuye import ContentBlock, ZhuyeAgent, ToolOutput


# ── K1 握手 ─────────────────────────────────────────────────────────────


def test_K1_spawn_握手成功即返回(env_home: Path) -> None:
    """spawn 内部等待 `session_configured` 才返回;超时/失败会直接抛。"""
    agent = ZhuyeAgent.spawn(make_options())
    agent.close()


# ── K2 单轮 / 多轮 prompt ────────────────────────────────────────────────


def test_K2a_单轮_prompt_文本回流(env_home: Path) -> None:
    """prompt() 聚合 delta 返回最终文本。"""
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"py-e2e-reply"}']))
    try:
        assert agent.prompt("第一轮") == "py-e2e-reply"
    finally:
        agent.close()


def test_K2b_多轮_prompt_上下文保持(env_home: Path) -> None:
    """两轮拿到 mock 脚本的第 1、2 行(脚本按模型调用顺序消耗)。"""
    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"text","text":"first-reply"}',
                '{"type":"text","text":"second-reply"}',
            ]
        )
    )
    try:
        assert agent.prompt("第一轮") == "first-reply"
        assert agent.prompt("第二轮") == "second-reply"
    finally:
        agent.close()


# ── K3 远程工具 ─────────────────────────────────────────────────────────


def test_K3_注册工具_本地执行_回执(env_home: Path) -> None:
    """注册 → LLM 调用 → 客户端 handler 执行 → 回执 → turn_complete。"""
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any]) -> ToolOutput:
        calls.append(args)
        return ToolOutput(
            content=[ContentBlock(type="text", text=f"echo:{args['q']}")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}',
                '{"type":"text","text":"tool-done"}',
            ]
        )
    )
    try:
        agent.register_tool("sdk_echo", "echo back", {"type": "object"}, handler)
        evs = drain(agent.submit("调工具"))
        assert calls == [{"q": "ping"}], f"handler 入参异常:{calls!r}"
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
    finally:
        agent.close()


# ── K4 非 turn op 收尾 ──────────────────────────────────────────────────


def test_K4_非turn_op_迭代器以submission_closed收尾(env_home: Path) -> None:
    """`submit_op` 发非 turn 操作:迭代器必须以 `submission_closed` 结束。

    没有它,SDK 的迭代器会永久挂起(v1.3 起 serve 在 per-turn 通道排空后
    发这条收尾标记)。
    """
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"t1"}']))
    try:
        evs = drain(agent.submit_op({"type": "set_effort", "effort": "high"}))
        assert msg_types(evs) == ["submission_closed"], f"收尾异常:{msg_types(evs)}"
    finally:
        agent.close()


# ── K5 interrupt ────────────────────────────────────────────────────────


def test_K5_interrupt_打断在飞turn(env_home: Path) -> None:
    """interrupt 打断停在远程工具等待上的 turn。

    wire 语义(实测):`turn_aborted` 落在**被打断的那个 turn** 的通道上
    (core 取消在飞回合 → 该回合以 `turn_aborted` 收尾);
    interrupt 这条 submission 自己的通道只收到收尾标记 `submission_closed`。
    """
    import threading

    handler_started = threading.Event()
    release_handler = threading.Event()

    def handler(args: dict[str, Any]) -> ToolOutput:
        handler_started.set()
        release_handler.wait(timeout=10)
        return ToolOutput(
            content=[ContentBlock(type="text", text="slow result")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"slow_tool","args":{}}',
                '{"type":"text","text":"after"}',
            ]
        )
    )
    try:
        agent.register_tool("slow_tool", "block until released", {"type": "object"}, handler)
        turn_events = agent.submit("调慢工具")
        assert handler_started.wait(timeout=10), "handler 未被调用"
        # interrupt 自己的通道:只收到收尾标记。
        intr_events = drain(
            agent.submit_op({"type": "interrupt"}, submission_id="intr-1")
        )
        assert msg_types(intr_events) == ["submission_closed"], (
            f"interrupt 通道应只有收尾标记:{msg_types(intr_events)}"
        )
        release_handler.set()
        # 被打断的 turn 以 turn_aborted 收尾。
        turn_types = msg_types(drain(turn_events))
        assert "turn_aborted" in turn_types, f"被打断的 turn 未收到 turn_aborted:{turn_types}"
    finally:
        release_handler.set()
        agent.close()


# ── K6 close 幂等 ───────────────────────────────────────────────────────


def test_K6_close_幂等(env_home: Path) -> None:
    """二次 close 不抛错。"""
    agent = ZhuyeAgent.spawn(make_options())
    agent.close()
    agent.close()


# ── K7 坏二进制 ─────────────────────────────────────────────────────────


def test_K7_坏二进制_握手失败抛错(env_home: Path) -> None:
    """from_popen 路径:进程吐非 JSONL → 握手失败抛 `RuntimeError` 而非挂死。"""
    with pytest.raises(RuntimeError):
        ZhuyeAgent.from_popen(
            subprocess.Popen(
                ["/bin/sh", "-c", "echo not-jsonl && sleep 5"],
                stdout=subprocess.PIPE,
            ),
            3.0,
        )


# ── K8 handler 异常 ─────────────────────────────────────────────────────


def test_K8_handler抛异常_转错误回执(env_home: Path) -> None:
    """handler 抛异常 → SDK 把它转成 `is_error=true` 的文本回执(turn 不挂死)。

    依据:sdks/PROTOCOL.md §5(SDK 内 handler 抛异常时应转成 is_error=true
    的文本回执,让 LLM 看到错误而不是挂死)。
    """

    def handler(args: dict[str, Any]) -> ToolOutput:
        raise RuntimeError("handler-boom")

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"boom_tool","args":{}}',
                '{"type":"text","text":"after-boom"}',
            ]
        )
    )
    try:
        agent.register_tool("boom_tool", "always raises", {"type": "object"}, handler)
        evs = drain(agent.submit("调会崩的工具"))
        ends = [ev for ev in evs if ev["msg"]["type"] == "tool_call_end"]
        assert ends, f"未见 tool_call_end:{msg_types(evs)}"
        assert ends[0]["msg"]["is_error"] is True, f"应回错误:{ends[0]['msg']}"
        text = "".join(
            c.get("text", "")
            for c in ends[0]["msg"].get("output", {}).get("content", [])
            if isinstance(c, dict)
        )
        assert "handler-boom" in text, f"错误文本未透传:{text!r}"
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
    finally:
        agent.close()


def test_K9_全局事件公开读取口(env_home: Path) -> None:
    """全局事件通过公开 API 读取,不会混入 submission 的事件迭代器。"""
    agent = ZhuyeAgent.spawn(make_options())
    try:
        handshake = agent.next_global_event(timeout_secs=5)
        assert handshake["id"] == ""
        assert handshake["msg"]["type"] == "session_configured"
        drain(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
        event = agent.next_global_event(timeout_secs=5)
        assert event["id"] == ""
        assert event["msg"]["type"] == "permission_mode_changed"
    finally:
        agent.close()
