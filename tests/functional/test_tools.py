"""能力覆盖层 · 工具面:内置工具、远程工具、审批链路。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- T1 内置工具调用(mock 调 `echo` → tool_call_begin → tool_call_end);
- T2 模型调未注册工具 → 错误输出(core 侧 `tool not found`);
- T3 远程工具往返(register_tools → tool_execution_request → 回执 → tool_call_end);
- T4 远程工具失败回执(is_error=true);
- T5 工具审批全链路(ZHUYE_APPROVALS=1 + MCP 工具);
- T6 审批 deny 分支(工具不执行 + 错误输出)。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_tools.py -v
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    MOCK_MCP,
    first_event,
    msg_types,
    require_binary,
)

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    """取 tool_call_end 事件(找不到直接失败,附上已见类型)。"""
    m = first_event(events, "tool_call_end")
    assert m is not None, f"未见 tool_call_end:{msg_types(events)}"
    return m["msg"]


def _tool_text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


# ── T 组:工具调用 ───────────────────────────────────────────────────────


def test_T1_内置工具调用(serve_session, mock_script) -> None:
    """mock 调内置 `echo` → tool_call_begin → tool_call_end(回显文本)。

    依据:tests/rust/tools_drive.sh 组 1(同名同参)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"echo","args":{"text":"echo-marker"}}',
            '{"type":"text","text":"tool-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo"}]},
            timeout=DRAIN_TIMEOUT,
        )
    begin = first_event(evs, "tool_call_begin")
    assert begin is not None, f"未见 tool_call_begin:{msg_types(evs)}"
    assert begin["msg"]["tool_name"] == "echo", f"tool_name={begin['msg']['tool_name']!r}"
    end = _tool_end(evs)
    assert end["is_error"] is False, f"echo 应成功:{end}"
    assert "echo-marker" in _tool_text(end), f"输出未含回显:{_tool_text(end)!r}"
    assert end.get("elapsed_ms", -1) >= 0, f"elapsed_ms 异常:{end}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T2_模型调未注册工具_错误输出(serve_session, mock_script) -> None:
    """模型调不存在的工具 → tool_call_end(is_error=true,含 `tool not found`)。

    依据:crates/abilities/zhuye-tools/src/queue.rs:296-304。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"no_such_tool_xyz","args":{}}',
            '{"type":"text","text":"recovered"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调不存在的工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(evs)
    assert end["is_error"] is True, f"未注册工具应报错:{end}"
    assert "tool not found" in _tool_text(end), f"错误文案异常:{_tool_text(end)!r}"
    # 工具失败不应打死 turn。
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T3_远程工具往返(serve_session, mock_script) -> None:
    """register_tools → 模型调用 → `tool_execution_request`(全局)→ 回执 → tool_call_end。

    依据:crates/abilities/zhuye-tools/src/remote.rs:66-110;
    sdks/PROTOCOL.md §5(远程工具执行流)。

    注意 call_id 的两套语义:`tool_execution_request.call_id` 是桥生成的
    uuid(回执用它配对),`tool_call_begin/end.call_id` 是模型的 tool id。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_echo","args":{"text":"hi-remote"}}',
            '{"type":"text","text":"after-tool"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_echo",
                        "description": "echo back",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调远程工具"}]}
        )
        req = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "tool_execution_request"
        )
        assert req["id"] == "", "远程工具请求是全局事件(id 为空)"
        rmsg = req["msg"]
        assert rmsg["tool"] == "remote_echo", f"tool={rmsg.get('tool')!r}"
        assert rmsg["args"] == {"text": "hi-remote"}, f"args={rmsg.get('args')!r}"
        # 回执用请求里的 call_id 配对。
        s.send(
            {
                "type": "tool_execution_response",
                "call_id": rmsg["call_id"],
                "output": {
                    "content": [{"type": "text", "text": "remote-ok"}],
                    "is_error": False,
                    "metadata": {},
                    "elapsed_ms": 1,
                },
            }
        )
        evs = s.collect(sid, timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert end["is_error"] is False, f"远程工具应成功:{end}"
    assert _tool_text(end) == "remote-ok", f"输出未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T4_远程工具失败回执(serve_session, mock_script) -> None:
    """回执 is_error=true → tool_call_end(is_error=true),turn 仍收尾。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_fail","args":{"boom":true}}',
            '{"type":"text","text":"remote-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_fail",
                        "description": "always fails",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调会失败的工具"}]}
        )
        req = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "tool_execution_request"
        )
        s.send(
            {
                "type": "tool_execution_response",
                "call_id": req["msg"]["call_id"],
                "output": {
                    "content": [{"type": "text", "text": "boom failed"}],
                    "is_error": True,
                    "metadata": {},
                    "elapsed_ms": 1,
                },
            }
        )
        evs = s.collect(sid, timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert end["is_error"] is True, f"失败回执应产出 is_error:{end}"
    assert "boom failed" in _tool_text(end), f"错误文本未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T5_工具审批全链路(serve_session, mock_script, sandbox) -> None:
    """MCP 工具(Prompt 权限)→ approval_request → 回执 → 工具真执行 → turn_complete。

    依据:crates/runtime/zhuye-core/src/submission_loop.rs:173-180
    (ZHUYE_APPROVALS=1 开启 gate)。
    """
    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")
    sandbox.write_config(
        f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
    )
    script = mock_script(
        [
            '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
            '{"type":"text","text":"approved-done"}',
        ]
    )
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        }
    ) as s:
        time.sleep(2.0)  # MCP 注册是异步的:握手 + list_tools 完成前调用会报 unknown tool
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo 工具"}]}
        )
        seen: dict[str, Any] = {}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request" and "request_id" not in seen:
                seen.update(m)
                s.send(
                    {
                        "type": "tool_approval",
                        "id": m["request_id"],
                        "decision": "approve",
                    }
                )

        evs = s.collect(sid, timeout=40.0, on_event=_on_event)
    assert seen, f"未见 approval_request:{msg_types(evs)}"
    assert seen["kind"]["type"] == "tool", f"kind={seen['kind']!r}"
    assert seen["kind"]["tool_name"] == "mcp__fsx__echo", f"tool_name={seen['kind']!r}"
    assert seen.get("risk") in ("low", "medium", "high"), f"缺 risk:{seen!r}"
    end = _tool_end(evs)
    assert end["is_error"] is False, f"放行后工具应成功:{end}"
    assert "probe" in _tool_text(end), f"输出未含回显:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"审批后 turn 未收尾:{msg_types(evs)}"


def test_T6_审批deny_工具不执行(serve_session, mock_script, sandbox) -> None:
    """审批回 deny → 工具**未执行**,tool_call_end(is_error=true),turn 仍收尾。

    依据:crates/abilities/zhuye-tools/src/approval/mod.rs:96-110。
    """
    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")
    sandbox.write_config(
        f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
    )
    script = mock_script(
        [
            '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
            '{"type":"text","text":"denied-done"}',
        ]
    )
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        }
    ) as s:
        time.sleep(2.0)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo 工具"}]}
        )
        seen: dict[str, Any] = {}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request" and "request_id" not in seen:
                seen.update(m)
                s.send(
                    {
                        "type": "tool_approval",
                        "id": m["request_id"],
                        "decision": {"deny": {"reason": "DENY-REASON-XYZ"}},
                    }
                )

        evs = s.collect(sid, timeout=40.0, on_event=_on_event)
    assert seen, f"未见 approval_request:{msg_types(evs)}"
    end = _tool_end(evs)
    assert end["is_error"] is True, f"deny 后工具应报错:{end}"
    assert "DENY-REASON-XYZ" in _tool_text(end), f"deny 理由未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"deny 后 turn 未收尾:{msg_types(evs)}"
