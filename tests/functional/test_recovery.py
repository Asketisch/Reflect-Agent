"""能力覆盖层 · 重连/恢复/容错。

直接驱动 `reflect serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- R 组 重连与恢复:`--resume` / `-c` / `-r N` 三入口;进程崩溃后客户端如何感知
  断开;stdin EOF 的优雅退出;
- X 组 容错与失败恢复:坏行不终止会话 / 空工具名 / 撞内置名 / 远程工具超时 /
  未知 op / 错误事件不污染 turn 流。

运行:tests/run_all.sh functional,或
  REFLECT_BIN=... python3 -m pytest tests/functional/test_recovery.py -v
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    TERMINAL_TYPES,
    first_event,
    msg_types,
    require_binary,
    rollout_records,
    rollout_thread_id,
)

pytestmark = pytest.mark.usefixtures("sandbox")


# ── R 组:重连与恢复 ─────────────────────────────────────────────────────


def test_R1_resume_按thread_id恢复会话(serve_session, mock_script, sandbox) -> None:
    """`reflect serve --resume <thread id>` → 恢复会话并**续写同一份** rollout。

    依据:crates/runtime/reflect-exec/src/serve.rs:57-65(headless.rs:602-748)。
    关键实现细节:`--resume` 的参数取**会话文件名**里的 thread id ——
    `session_configured.session_id` 与它是两个不同的 uuid。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"first-reply"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)  # 落盘是 best-effort spawn,给 writer 一点时间
    tid = rollout_thread_id(sandbox.home)
    before = len(rollout_records(sandbox.home))

    script2 = mock_script(['{"type":"text","text":"second-reply"}'])
    with serve_session(
        args=["--resume", tid],
        env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script2)},
    ) as s2:
        evs = s2.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = "".join(
        e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
    )
    assert text == "second-reply", f"恢复后第二轮文本={text!r}"
    assert "turn_complete" in msg_types(evs), f"恢复后 turn 未收尾:{msg_types(evs)}"

    time.sleep(0.8)
    recs = rollout_records(sandbox.home)
    assert len(recs) > before, "resume 后应续写同一份 rollout(记录数增加)"
    blob = "\n".join(str(r) for r in recs)
    assert "第一轮" in blob, "resume 后历史消息应仍在"
    assert "第二轮" in blob, "resume 后新消息应已落盘"


def test_R2_continue_last_续最近会话(serve_session, mock_script, sandbox) -> None:
    """`reflect serve -c` → 续最近一次会话。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"first-reply"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    script2 = mock_script(['{"type":"text","text":"resumed-reply"}'])
    with serve_session(
        args=["-c"], env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script2)}
    ) as s2:
        evs = s2.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "续"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = "".join(
        e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
    )
    assert text == "resumed-reply", f"-c 续会话文本={text!r}"


def test_R3_resume_by_index_按序号恢复(serve_session, mock_script, sandbox) -> None:
    """`reflect serve -r 1` → 按序号(1 = 最近)恢复会话。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"first-reply"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    script2 = mock_script(['{"type":"text","text":"resumed-by-index"}'])
    with serve_session(
        args=["-r", "1"],
        env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script2)},
    ) as s2:
        evs = s2.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "续"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = "".join(
        e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
    )
    assert text == "resumed-by-index", f"-r 1 续会话文本={text!r}"


def test_R4_进程崩溃_客户端靠EOF感知断开(serve_session, mock_script) -> None:
    """进程被强杀:挂起的 submission **收不到终态事件**,客户端靠 stdout EOF 感知断开。

    依据:serve.rs 的 writer 任务随进程消失;wire 上没有"会话结束"事件 ——
    这是协议层事实,SDK 侧靠 EOF 唤醒挂起消费者(见 tests/sdk-python 绑定层)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_slow","args":{}}',
            '{"type":"text","text":"never"}',
        ]
    )
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_slow",
                        "description": "slow",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调慢工具"}]}
        )
        s.wait_global(lambda e: e.get("msg", {}).get("type") == "tool_execution_request")
        s.kill()
        # 崩溃后:该 submission 不应收到任何终态事件。
        time.sleep(1.5)
        evs = [e for e in s.all_events() if e.get("id") == sid]
        got = [e["msg"]["type"] for e in evs]
        assert not (set(got) & TERMINAL_TYPES), f"崩溃后不应有终态事件:{got}"
        # stdout 关闭(EOF)→ 客户端可判定会话断开。
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if s._proc.stdout is not None and s._proc.stdout.closed:
                break
            if s._proc.poll() is not None:
                break
            time.sleep(0.1)
        assert s._proc.poll() is not None, "崩溃后进程应已退出(EOF 可达)"


def test_R5_stdin_EOF_优雅退出(serve_session, mock_script) -> None:
    """stdin EOF 等价于 shutdown:进程退出码 0。

    依据:sdks/PROTOCOL.md §6、crates/runtime/reflect-exec/src/serve.rs:194-202。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"x"}'])
    s = serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)})
    assert s._proc.stdin is not None
    s._proc.stdin.close()
    try:
        rc = s._proc.wait(timeout=15)
    except Exception as exc:  # pragma: no cover - 失败路径
        pytest.fail(f"EOF 后进程未在 15s 内退出:{exc}")
    assert rc == 0, f"EOF 退出码={rc}"


# ── X 组:容错与失败恢复 ─────────────────────────────────────────────────


def test_X1_坏行不终止会话(serve_session, mock_script) -> None:
    """写入非法 JSON → 全局 error(invalid_submission)→ 会话存活,下一轮正常。

    依据:crates/runtime/reflect-exec/src/serve.rs:207-224、sdks/PROTOCOL.md §2。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"after-bad"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.write_line("this-is-not-json")
        err = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "error"
            and e["msg"].get("code") == "invalid_submission"
        )
        assert err["id"] == "", "错误事件是全局事件(id 为空)"
        assert err["msg"].get("details", {}).get("line"), f"缺 details.line:{err['msg']}"
        # 会话存活:下一轮 prompt 正常回流。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = "".join(
        e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
    )
    assert text == "after-bad", f"坏行后会话未存活:{text!r}"


def test_X2_空工具名被拒(serve_session, mock_script) -> None:
    """register_tools 带空工具名 → 全局 error(tool_name_invalid)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {"name": "", "description": "空名", "parameters": {"type": "object"}}
                ],
            }
        )
        err = s.wait_global(
            lambda e: e.get("msg", {}).get("code") == "tool_name_invalid"
        )
        assert err["msg"]["code"] == "tool_name_invalid", f"code={err['msg']['code']!r}"
        # 会话存活。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]},
            timeout=DRAIN_TIMEOUT,
        )
    assert "turn_complete" in msg_types(evs), f"会话未存活:{msg_types(evs)}"


def test_X3_工具名撞内置被拒(serve_session, mock_script) -> None:
    """register_tools 用内置工具名(bash)→ 全局 error(tool_name_conflict)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "bash",
                        "description": "撞内置名",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        err = s.wait_global(
            lambda e: e.get("msg", {}).get("code") == "tool_name_conflict"
        )
        assert err["msg"]["code"] == "tool_name_conflict", f"code={err['msg']['code']!r}"
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]},
            timeout=DRAIN_TIMEOUT,
        )
    assert "turn_complete" in msg_types(evs), f"会话未存活:{msg_types(evs)}"


def test_X4_远程工具超时_以错误输出收尾(serve_session, mock_script) -> None:
    """不回执 → 超时后工具以错误输出收尾(is_error + `timed out`),turn 不挂死。

    依据:crates/abilities/reflect-tools/src/remote.rs:96-103。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_slow","args":{}}',
            '{"type":"text","text":"after-slow"}',
        ]
    )
    with serve_session(
        env={
            "REFLECT_MODEL": "mock",
            "REFLECT_MOCK_SCRIPT": str(script),
            "REFLECT_REMOTE_TOOL_TIMEOUT_SECS": "1",
        }
    ) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_slow",
                        "description": "slow",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调慢工具"}]},
            timeout=40.0,
        )
    end = first_event(evs, "tool_call_end")
    assert end is not None, f"未见 tool_call_end:{msg_types(evs)}"
    assert end["msg"]["is_error"] is True, f"超时应报错:{end['msg']}"
    text = "".join(
        c.get("text", "")
        for c in end["msg"].get("output", {}).get("content", [])
        if isinstance(c, dict)
    )
    assert "timed out" in text, f"错误文案缺 timed out:{text!r}"
    assert "turn_complete" in msg_types(evs), f"超时后 turn 未收尾:{msg_types(evs)}"


def test_X5_未知op不炸会话(serve_session, mock_script) -> None:
    """合法 JSON 但未知 op.type → error(invalid_submission),会话存活。

    协议 v0 冻结:未知变体是客户端义务;服务端按 serde 报错但**不终止会话**。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"after-unknown"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.write_line('{"id":"u-unknown","op":{"type":"totally_unknown_op"}}')
        err = s.wait_global(
            lambda e: e.get("msg", {}).get("code") == "invalid_submission"
        )
        assert err["msg"]["code"] == "invalid_submission", f"code={err['msg']['code']!r}"
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = "".join(
        e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
    )
    assert text == "after-unknown", f"未知 op 后会话未存活:{text!r}"


def test_X6_错误事件不污染turn流(serve_session, mock_script) -> None:
    """全局 error(id=="")不进按 id 路由的 turn 事件流。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.write_line("this-is-not-json")
        s.wait_global(lambda e: e.get("msg", {}).get("code") == "invalid_submission")
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "正常一轮"}]}
        )
        evs = s.collect(sid, timeout=DRAIN_TIMEOUT)
    turn_ids = {e.get("id") for e in evs}
    assert all(i == sid for i in turn_ids), f"turn 流混入了别的 id:{turn_ids}"
    assert "error" not in msg_types(evs), f"turn 流混入 error:{msg_types(evs)}"
    globals_ = [e for e in s.global_events() if e["msg"]["type"] == "error"]
    assert globals_, "错误应在全局事件里"
    assert all(e.get("id") == "" for e in globals_), "错误事件应为全局(id 为空)"
