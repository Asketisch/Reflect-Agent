"""reflect-agent Python SDK 测试:用本地 Node "mock serve" 驱动协议。

不依赖真 `reflect` 二进制 —— mock 子进程用 Node 简单回显 JSONL,
覆盖 SDK 的协议回路(handshake / submit / prompt / register_tool /
shutdown)。
"""

from __future__ import annotations

import json
import queue
import shutil
import subprocess
import sys
import time
from typing import Any

import pytest

from reflect import ReflectAgent, ToolOutput, ContentBlock


MOCK_SERVE_JS = r"""
// 必须用 console.log (sync flush); process.stdout.write 是 async flush,
// 在 Node 上会让 Python readline() 在 pipe EOF 之前拿不到任何数据。
console.log(JSON.stringify({
  id: '',
  msg: {
    type: 'session_configured',
    session_id: 'mock-session',
    model: 'mock/mock-1',
    provider: 'mock',
    approval_policy: 'auto',
    sandbox_policy: 'workspace_only',
  },
}));
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
// 未收到 shutdown 就关闭 stdin 属于异常退出,用于验证客户端关闭流程。
rl.on('close', () => process.exit(23));
rl.on('line', (line) => {
  let sub;
  try { sub = JSON.parse(line); } catch { return; }
  const opType = sub.op && sub.op.type;
  if (opType === 'shutdown') {
    console.log(JSON.stringify({ id: sub.id, msg: { type: 'shutdown_complete' } }));
    process.exit(0);
    return;
  }
  if (opType === 'register_tools') {
    return;
  }
  // 测试注入:让 mock 以指定 submission id emit 一条事件(模拟
  // core 下发的 tool_execution_request 等),用于验证 SDK 侧路由与回执。
  if (opType === 'inject_event') {
    console.log(JSON.stringify({ id: sub.id, msg: sub.op.event }));
    return;
  }
  // 测试回执:把客户端写回的 ToolOutput 以全局事件(id='')回显,
  // 供测试断言 SDK 确实调用了本地 handler 并写入了正确回执。
  // 注:回执 submission 的 id 是 SDK 新生成的 uuid(无监听者),
  // 故 echo 走全局事件通道。
  if (opType === 'tool_execution_response') {
    console.log(JSON.stringify({
      id: '',
      msg: {
        type: 'tool_echo',
        call_id: sub.op.call_id,
        output: sub.op.output,
      },
    }));
    return;
  }
  if (opType === 'user_input' || opType === 'interrupt') {
    console.log(JSON.stringify({
      id: sub.id, msg: { type: 'turn_started', turn_id: 't', user_message_id: 'm' },
    }));
    console.log(JSON.stringify({
      id: sub.id, msg: { type: 'agent_message_delta', delta: 'mock-reply' },
    }));
    // v1.5 additive 变体:工具参数增量(SDK 应原样透传给客户端)。
    console.log(JSON.stringify({
      id: sub.id, msg: {
        type: 'tool_call_args_delta', call_id: 'c1', delta: '{"text":"pi',
      },
    }));
    console.log(JSON.stringify({
      id: sub.id, msg: { type: 'tool_call_output_delta', call_id: 'c1', delta: 'preview', is_stderr: true },
    }));
    // 未知变体:SDK 不得因不认识而抛错(按未知 type 降级)。
    console.log(JSON.stringify({
      id: sub.id, msg: { type: 'some_future_event', payload: { x: 1 } },
    }));
    console.log(JSON.stringify({
      id: sub.id, msg: {
        type: 'turn_complete', turn_id: 't', status: 'success',
        usage: { input_tokens: 0, output_tokens: 1 },
      },
    }));
  }
});
"""


def _spawn_mock() -> subprocess.Popen:
    node = shutil.which("node")
    if not node:
        pytest.skip("node not available for mock serve")
    return subprocess.Popen(
        [node, "-e", MOCK_SERVE_JS],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


@pytest.fixture
def agent() -> ReflectAgent:
    proc = _spawn_mock()
    try:
        ag = ReflectAgent.from_popen(proc, handshake_timeout_secs=30.0)
    except Exception:
        proc.kill()
        raise
    yield ag
    if not ag._closed:
        ag.close()


def test_prompt_aggregates_deltas(agent: ReflectAgent) -> None:
    assert agent.prompt("hi") == "mock-reply"


def test_register_tool_then_submit_iterates_turn_events(agent: ReflectAgent) -> None:
    calls = []

    def handler(args):
        calls.append(args)
        return ToolOutput(
            content=[ContentBlock(type="text", text="x")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent.register_tool("foo", "foo tool", {"type": "object"}, handler)
    events = list(agent.submit("test"))
    types = [ev["msg"]["type"] for ev in events]
    assert "turn_started" in types
    assert "agent_message_delta" in types
    assert types[-1] == "turn_complete"
    # register_tools 被静默接受(无回执事件)。
    assert calls == []


def test_close_sends_shutdown(agent: ReflectAgent) -> None:
    # mock 在收到 shutdown 时退出 0,只收到 EOF 时退出 23。
    agent.close()
    agent.close()
    assert agent._closed is True
    assert agent._proc.returncode == 0
    with pytest.raises(RuntimeError, match="agent is closed"):
        agent.submit("closed")


def test_tool_execution_response_handled(agent: ReflectAgent) -> None:
    """喂一个全局(id="")的 tool_execution_request:SDK 应在 reader 线程外
    调本地 handler 并写回正确回执(与真实 serve 的 wire 行为一致,
    见 PROTOCOL.md §5)。mock 把回执以全局 `tool_echo` 事件回显供断言。
    """
    received = {}

    def handler(args):
        received.update(args)
        return ToolOutput(
            content=[ContentBlock(type="text", text=f"echo:{args.get('q')}")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent.register_tool("echo", "echo back", {"type": "object"}, handler)
    # 注入 id="" 的全局事件(真实 serve 的远程工具请求即此形态)。
    agent.submit_op(
        {
            "type": "inject_event",
            "event": {
                "type": "tool_execution_request",
                "call_id": "c1",
                "tool": "echo",
                "args": {"q": "hi"},
            },
        },
        submission_id="sub-inject",
    )
    # 等 mock 回显的 tool_echo 入全局队列。
    deadline = time.monotonic() + 5.0
    echo: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        try:
            ev = agent.next_global_event(timeout_secs=0.1)
        except queue.Empty:
            continue
        if ev is not None and ev.get("msg", {}).get("type") == "tool_echo":
            echo = ev
            break
    assert echo is not None, "5s 内未收到 mock 的 tool_echo 回显"
    assert echo["msg"]["call_id"] == "c1"
    assert echo["msg"]["output"]["content"][0]["text"] == "echo:hi"
    assert echo["msg"]["output"]["is_error"] is False
    assert received == {"q": "hi"}


def test_tool_call_args_delta_透传到事件流(agent: ReflectAgent) -> None:
    """v1.5:additive 变体 `tool_call_args_delta` 应原样出现在 turn 事件流里。

    协议变更是 additive 也必须同步 SDK —— 客户端要能拿到工具参数增量做
    渐进渲染;字段名保持 snake_case,与 Rust serde 一致。
    """
    events = list(agent.submit("hi"))
    types = [ev["msg"]["type"] for ev in events]
    assert "tool_call_args_delta" in types, f"未见 tool_call_args_delta:{types}"
    d = next(ev["msg"] for ev in events if ev["msg"]["type"] == "tool_call_args_delta")
    assert d["call_id"] == "c1", f"call_id 不对:{d}"
    assert d["delta"] == '{"text":"pi', f"delta 不对:{d}"


def test_未知事件变体降级不崩(agent: ReflectAgent) -> None:
    """协议新增变体时,SDK 不得因不认识而抛错(按未知 type 降级)。"""
    events = list(agent.submit("hi"))
    types = [ev["msg"]["type"] for ev in events]
    assert "some_future_event" in types, f"未知变体应被原样透传:{types}"
    assert types[-1] == "turn_complete", f"未知变体不应打断收尾:{types}"
    # agent 仍可用。
    assert agent.prompt("hi") == "mock-reply"


def test_wire_field_names_stable() -> None:
    """核心 wire 字段名 / 判别符保持 snake_case,与 Rust serde 一致。"""
    sub = {"id": "x", "op": {"type": "user_input", "items": [{"type": "text", "text": "hi"}]}}
    enc = json.dumps(sub)
    assert "user_input" in enc
    assert "UserInput" not in enc


def test_tool_output_wire_format() -> None:
    out = ToolOutput(
        content=[ContentBlock(type="text", text="hi")],
        is_error=False,
        metadata={},
        elapsed_ms=0,
    )
    enc = json.dumps(out)
    assert "is_error" in enc
    assert "elapsed_ms" in enc
    assert "isError" not in enc
    assert "elapsedMs" not in enc

def test_optional_fields_preserve_required_keys() -> None:
    """零依赖类型声明保留必填与可选字段,不依赖 Python 3.11 的 NotRequired。"""
    from reflect import protocol

    assert protocol.UserInputOp.__required_keys__ == {"type", "items"}
    assert protocol.UserInputOp.__optional_keys__ == {"thread_settings"}
    assert protocol.Submission.__required_keys__ == {"id", "op"}
    assert protocol.Submission.__optional_keys__ == {"client_user_message_id"}
    assert protocol.ThreadSettings.__required_keys__ == set()
    assert protocol.ThreadSettings.__optional_keys__ == {"effort", "permission_mode", "model"}
    assert protocol.ErrorEvent.__required_keys__ == {"type", "code", "message"}
    assert protocol.ErrorEvent.__optional_keys__ == {"details"}


def test_global_events_and_eof(agent: ReflectAgent) -> None:
    """全局事件有公开读取口,EOF 后重复读取也不会挂起。"""
    event = agent.next_global_event(timeout_secs=1)
    assert event["msg"]["type"] == "session_configured"
    with pytest.raises(queue.Empty):
        agent.next_global_event(timeout_secs=0.01)
    agent.close()
    assert agent.next_global_event(timeout_secs=1) is None
    assert agent.next_global_event(timeout_secs=0.01) is None


def test_tool_output_delta_forwarded(agent: ReflectAgent) -> None:
    """输出增量经过 SDK 路由后保持字段与顺序。"""
    messages = [e["msg"] for e in agent.submit("hi")]
    delta = next(m for m in messages if m["type"] == "tool_call_output_delta")
    assert delta == {"type": "tool_call_output_delta", "call_id": "c1", "delta": "preview", "is_stderr": True}
    assert messages[-1]["type"] == "turn_complete"


@pytest.mark.skipif(sys.version_info < (3, 10), reason="联合类型求值需要受支持的 Python 版本")
def test_wire_type_annotations() -> None:
    """公开注解必须描述 Rust 实际发送与接收的 wire 值。"""
    from typing import get_args, get_type_hints
    from reflect import protocol

    assert set(get_args(get_type_hints(protocol.TurnComplete)["status"])) == {"success", "max_iterations", "stopped", "token_budget_exceeded"}
    assert set(get_args(get_type_hints(protocol.PlanApprovalOp)["choice"])) == {"auto_mode", "manual_approve", "revise"}
    assert get_type_hints(protocol.TurnAborted)["reason"] == protocol.AbortReason
    assert protocol.ReviewDecisionDeny.__required_keys__ == {"deny"}
