"""能力覆盖层 · 协议面:基础通信、并发路由、19 Op 会话状态面。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK),断言到**字段级**。

覆盖:
- D 组 基础通信与事件流:握手 / 单轮事件序列 / 多轮上下文 / token_count / 终态语义 /
  工具参数增量事件(`tool_call_args_delta`);
- S 组 并发与路由:submission 隔离 / 队列串行 / 收尾不变量;
- A 组 会话状态 Op:user_input+thread_settings / set_effort / 权限模式 / rewind /
  interrupt / compact / goal 模式 / plan 模式 / 四种孤儿回执 / steer /
  query_subagents / prompt_command。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_serve.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    MOCK_MCP,
    ServeSession,
    first_event,
    msg_types,
    require_binary,
    rollout_records,
)

pytestmark = pytest.mark.usefixtures("sandbox")


# ── D 组:基础通信与事件流 ───────────────────────────────────────────────


def test_D1_握手事件字段完整(serve_session, mock_script) -> None:
    """serve 启动即主动 emit 全局 `session_configured`(id=="")。

    依据:crates/runtime/zhuye-exec/src/serve.rs:417-433
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"hi"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        ev = s.wait_global(lambda e: e.get("msg", {}).get("type") == "session_configured")
        assert ev["id"] == "", "握手事件应是全局事件(id 为空)"
        m = ev["msg"]
        assert m["model"] == "mock/mock-1", f"model={m.get('model')!r}"
        assert m["provider"] == "mock", f"provider={m.get('provider')!r}"
        assert m["session_id"], "session_id 不应为空"
        assert m["approval_policy"] == "auto"
        assert m["sandbox_policy"] == "workspace_only"


def test_D2_单轮事件序列与字段(serve_session, mock_script) -> None:
    """一轮的完整事件流:turn_started → delta×2 → token_count → turn_complete。

    依据:mock.rs:208-217(文本拆两段 delta)、event_msg/turn.rs:75-85
    (status 的 wire 真值是 `success`)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"hello-from-mock"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    types = msg_types(evs)
    # turn 事件以 turn_complete 收尾;其后可能还有一条收尾标记
    # `submission_closed`(serve 在 per-turn 通道排空后发出)。
    turn_types = [t for t in types if t != "submission_closed"]
    assert turn_types[0] == "turn_started", f"事件流应以 turn_started 起:{types}"
    assert turn_types[-1] == "turn_complete", f"turn 事件应以 turn_complete 收:{types}"
    deltas = [e for e in evs if e["msg"]["type"] == "agent_message_delta"]
    assert len(deltas) == 2, f"mock 把文本拆成两段 delta,实得 {len(deltas)}"
    assert "".join(d["msg"]["delta"] for d in deltas) == "hello-from-mock"
    tc = evs[-1]["msg"] if evs[-1]["msg"]["type"] == "turn_complete" else first_event(
        evs, "turn_complete"
    )["msg"]
    assert tc["status"] == "success", f"turn_complete.status={tc.get('status')!r}"
    usage = tc["usage"]
    for f in ("input_tokens", "output_tokens", "cached_tokens", "total_tokens"):
        assert f in usage, f"usage 缺字段 {f}:{usage}"
    started = first_event(evs, "turn_started")["msg"]
    assert started["turn_id"], "turn_started 缺 turn_id"
    assert tc["turn_id"] == started["turn_id"], "turn_complete 的 turn_id 应与 turn_started 一致"


def test_D3_多轮上下文保持(serve_session, mock_script) -> None:
    """两轮各自拿到 mock 脚本的第 1、2 行(脚本按模型调用顺序消耗)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"first-reply"}',
            '{"type":"text","text":"second-reply"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        first = s.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
        second = s.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
    text1 = "".join(
        e["msg"]["delta"] for e in first if e["msg"]["type"] == "agent_message_delta"
    )
    text2 = "".join(
        e["msg"]["delta"] for e in second if e["msg"]["type"] == "agent_message_delta"
    )
    assert text1 == "first-reply", f"第一轮={text1!r}"
    assert text2 == "second-reply", f"第二轮={text2!r}"


def test_D4_token_count_带模型维度(serve_session, mock_script) -> None:
    """一轮内出现 token_count,且带 model / provider 维度(v1.x stats 字段)。

    依据:event_msg/agent.rs:35-56(`model`/`provider` 为 serde default 的
    可选字段,缺省时不序列化)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"tok"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "计数"}]})
    tcs = [e["msg"] for e in evs if e["msg"]["type"] == "token_count"]
    assert tcs, f"未见 token_count 事件:{msg_types(evs)}"
    for f in ("input_tokens", "output_tokens"):
        assert f in tcs[0], f"token_count 缺 {f}"
    # 至少一条带模型维度(模型级统计的入口)。
    assert any(tc.get("model") == "mock/mock-1" for tc in tcs), f"无 model 维度:{tcs}"


def test_D9_工具参数增量事件字段完整(serve_session, mock_script) -> None:
    """模型生成工具参数期间逐段转发 `tool_call_args_delta`(v1.5 additive 变体)。

    依据:protocol/.../event_msg/tool.rs 的 `ToolCallArgsDeltaEvent`
    (call_id + delta);core/.../model_call/stream.rs 把 LLM 层的
    `ToolUseDelta` 按进行中的 call_id 转发给客户端。

    mock 用 `tool_call_delta` 脚本行制造**分片形态**参数下发
    (Anthropic / OpenAI 的「start → 一/多个 delta → stop」)—— 整包形态
    (参数全在 ToolUseStart 里)没有 delta 可转发,看不到本事件。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call_delta","name":"echo","chunks":["{\\"text\\":\\"pi","ng\\"}"]}',
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "调工具"}]})
    types = msg_types(evs)
    deltas = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_args_delta"]
    assert deltas, f"未见 tool_call_args_delta 事件:{types}"
    # 字段级:call_id + delta 两个字段都在,且非空。
    for d in deltas:
        assert "call_id" in d, f"缺 call_id:{d}"
        assert "delta" in d, f"缺 delta:{d}"
        assert d["call_id"], f"call_id 不应为空:{d}"
        assert d["delta"], f"delta 不应为空:{d}"
    # 分片按到达顺序拼接 = 完整参数 JSON。
    joined = "".join(d["delta"] for d in deltas)
    assert joined == '{"text":"ping"}', f"分片拼接结果={joined!r}"
    # call_id 与 tool_call_begin 一致(同一调用)。
    begins = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_begin"]
    assert begins, f"未见 tool_call_begin:{types}"
    assert deltas[0]["call_id"] == begins[0]["call_id"], (
        f"args_delta 的 call_id 应与 tool_call_begin 一致:{deltas[0]} vs {begins[0]}"
    )
    # 时序:增量在 tool_call_begin 之后(客户端先知道有这通调用,再收参数)。
    assert types.index("tool_call_args_delta") > types.index("tool_call_begin"), (
        f"args_delta 应在 tool_call_begin 之后:{types}"
    )


def test_D5_终态语义_turn后不再有事件(serve_session, mock_script) -> None:
    """turn 类 submission 以 `turn_complete` 收尾;非 turn 类以 `submission_closed` 收尾。

    依据:serve.rs:308-317(per-turn 通道排空后发收尾标记)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}', '{"type":"text","text":"t2"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        turn_types = [t for t in msg_types(turn) if t != "submission_closed"]
        assert turn_types[-1] == "turn_complete", f"turn 应以 turn_complete 收:{msg_types(turn)}"
        # turn_complete 之后不再有该 submission 的 turn 事件。
        idx = msg_types(turn).index("turn_complete")
        after = [t for t in msg_types(turn)[idx + 1 :] if t != "submission_closed"]
        assert not after, f"turn_complete 后仍有事件:{after}"
        # 非 turn 操作:set_effort 静默,只应有收尾标记。
        eff = s.submit({"type": "set_effort", "effort": "high"})
        assert msg_types(eff) == ["submission_closed"], f"set_effort 应静默:{msg_types(eff)}"


# ── S 组:并发与路由 ─────────────────────────────────────────────────────


def test_S1_并发_submission_事件不串台(serve_session, mock_script) -> None:
    """连发两条 user_input:两条 submission 各自成 turn,事件按 id 路由**互不干扰**。

    依据:sdks/PROTOCOL.md §4(各 submission 的事件以各自 id 路由);
    core 侧每个 `UserInput` 提交 `tokio::spawn` 一个独立 task
    (submission_loop.rs:621)—— 两个 turn 是**真并发**,模型调用的先后
    不保证,所以断言只要求"各流自洽 + 两条回复各用一次"。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"reply-A"}',
            '{"type":"text","text":"reply-B"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 不等第一个结束就发第二个。
        id_a = s.send({"type": "user_input", "items": [{"type": "text", "text": "A"}]})
        id_b = s.send({"type": "user_input", "items": [{"type": "text", "text": "B"}]})
        evs_a = s.collect(id_a)
        evs_b = s.collect(id_b)
    # 路由隔离:每条流里的事件 id 全属于自己的 submission。
    assert {e.get("id") for e in evs_a} == {id_a}, f"A 流混入别的 id"
    assert {e.get("id") for e in evs_b} == {id_b}, f"B 流混入别的 id"
    # 各自成 turn:两个不同的 turn_id,互不合并。
    turn_a = first_event(evs_a, "turn_started")["msg"]["turn_id"]
    turn_b = first_event(evs_b, "turn_started")["msg"]["turn_id"]
    assert turn_a != turn_b, "两个 submission 应各成一个 turn"
    # 两条 mock 回复各用一次(并发下先后不保证)。
    texts = [
        "".join(
            e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
        )
        for evs in (evs_a, evs_b)
    ]
    assert sorted(texts) == ["reply-A", "reply-B"], f"回复分配异常:{texts}"


def test_S2_并发_每个turn独立收尾(serve_session, mock_script) -> None:
    """并发提交的两个 turn 各自以 `turn_complete` 收尾,互不吞并。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"reply-A"}',
            '{"type":"text","text":"reply-B"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        id_a = s.send({"type": "user_input", "items": [{"type": "text", "text": "A"}]})
        id_b = s.send({"type": "user_input", "items": [{"type": "text", "text": "B"}]})
        evs_a = s.collect(id_a)
        evs_b = s.collect(id_b)
    for label, evs in (("A", evs_a), ("B", evs_b)):
        types = msg_types(evs)
        assert types.count("turn_started") == 1, f"{label} 应恰好一个 turn_started:{types}"
        assert types.count("turn_complete") == 1, f"{label} 应恰好一个 turn_complete:{types}"
        tc = first_event(evs, "turn_complete")["msg"]
        assert tc["status"] == "success", f"{label} turn 状态异常:{tc}"


def test_S3_每条submission恰好一条收尾标记(serve_session, mock_script) -> None:
    """不变量:每条 submission 的 per-turn 通道排空后恰好一条 `submission_closed`。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        s.submit({"type": "set_effort", "effort": "low"})
        s.submit({"type": "compact"})
        # 等收尾标记都落地(turn 类的收尾标记在 turn_complete 之后到达)。
        import time

        time.sleep(1.0)
        events = s.all_events()
    per_sub: dict[str, int] = {}
    for e in events:
        if e["msg"]["type"] == "submission_closed":
            per_sub[e.get("id", "")] = per_sub.get(e.get("id", ""), 0) + 1
    assert per_sub, f"未见任何 submission_closed:{msg_types(events)}"
    assert all(v == 1 for v in per_sub.values()), f"收尾标记重复:{per_sub}"


# ── A 组:会话状态 Op ────────────────────────────────────────────────────


def test_A1_user_input_带thread_settings(serve_session, mock_script) -> None:
    """thread_settings 四字段下发 → turn 正常完成、不产生 error 事件。

    依据:tests/rust/serve_ops.sh:101-112
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {
                "type": "user_input",
                "items": [{"type": "text", "text": "带设置的一轮"}],
                "thread_settings": {
                    "model": "mock",
                    "approval_policy": "prompt",
                    "sandbox_policy": "workspace_only",
                    "max_tool_concurrency": 2,
                },
            }
        )
    assert "turn_complete" in msg_types(evs), f"turn 未完成:{msg_types(evs)}"
    errs = [e for e in evs if e["msg"]["type"] == "error"]
    assert not errs, f"thread_settings 不应产生 error:{errs}"


def test_A2_set_effort_静默(serve_session, mock_script) -> None:
    """set_effort 只写槽,不产任何 turn 事件(仅收尾标记)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "set_effort", "effort": "high"})
    assert msg_types(evs) == ["submission_closed"], f"set_effort 应静默:{msg_types(evs)}"


def test_A3_权限模式切换_全局事件与持久化轨迹(serve_session, mock_script, sandbox) -> None:
    """set_permission_mode / cycle_permission_mode → 全局 `permission_mode_changed`。

    依据:submission_loop.rs:110-118;持久化见 COVERAGE.md §4。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        s.submit({"type": "set_permission_mode", "mode": "plan"})
        s.submit({"type": "cycle_permission_mode"})
        pmc = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "permission_mode_changed"
        )
        # cycle 会再发一条,等轨迹齐全。
        import time

        time.sleep(1.0)
        all_pmc = [
            e["msg"]
            for e in s.global_events()
            if e["msg"]["type"] == "permission_mode_changed"
        ]
    assert pmc["id"] == "", "permission_mode_changed 是全局事件(id 为空)"
    transitions = [(m.get("from"), m.get("to")) for m in all_pmc]
    assert ("auto", "plan") in transitions, f"缺 auto→plan:{transitions}"
    assert ("plan", "auto") in transitions, f"缺 plan→auto:{transitions}"
    # 持久化轨迹(rollout JSONL)。
    recs = rollout_records(sandbox.home)
    pmc_recs = [r for r in recs if r.get("type") == "permission_mode_changed"]
    assert pmc_recs, f"rollout 无 permission_mode_changed 记录:{[r.get('type') for r in recs]}"
    assert all(r.get("at") for r in pmc_recs), f"pmc 记录缺 at:{pmc_recs}"


def test_A4_rewind_回退(serve_session, mock_script) -> None:
    """rewind → `turn_rewound`(带 truncated_after;to_turn_id 可空)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"one"}',
            '{"type":"text","text":"two"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "rewind"})
    m = first_event(evs, "turn_rewound")
    assert m is not None, f"未见 turn_rewound:{msg_types(evs)}"
    assert isinstance(m["msg"].get("truncated_after"), int), f"缺 truncated_after:{m['msg']}"
    assert "to_turn_id" in m["msg"], f"缺 to_turn_id:{m['msg']}"


def test_A5_interrupt_打断(serve_session, mock_script) -> None:
    """interrupt → `turn_aborted`,reason 是带 type 的对象。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "interrupt"})
    m = first_event(evs, "turn_aborted")
    assert m is not None, f"未见 turn_aborted:{msg_types(evs)}"
    reason = m["msg"].get("reason")
    assert isinstance(reason, dict), f"reason 应为对象:{reason!r}"
    assert reason.get("type") == "user_interrupt", f"reason.type={reason.get('type')!r}"


def test_A6_compact_压缩(serve_session, mock_script) -> None:
    """compact → `context_compacted`(带 strategy)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "compact"}, timeout=40.0)
    m = first_event(evs, "context_compacted")
    assert m is not None, f"未见 context_compacted:{msg_types(evs)}"
    assert "strategy" in m["msg"], f"缺 strategy:{m['msg']}"


def test_A7_goal模式_进出静默且turn正常(serve_session, mock_script) -> None:
    """enter/exit_goal_mode 均静默(仅收尾标记);进出后一轮 turn 正常完成。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"t1"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        enter = s.submit(
            {
                "type": "enter_goal_mode",
                "goal": "probe goal",
                "token_budget": 100000,
            }
        )
        exit_ = s.submit({"type": "exit_goal_mode"})
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "干活"}]})
    assert msg_types(enter) == ["submission_closed"], f"enter_goal 应静默:{msg_types(enter)}"
    assert msg_types(exit_) == ["submission_closed"], f"exit_goal 应静默:{msg_types(exit_)}"
    assert "turn_complete" in msg_types(turn), f"goal 模式后 turn 未完成:{msg_types(turn)}"


def test_A8_plan模式_全链路(serve_session, mock_script) -> None:
    """enter_plan_mode → plan_request;exit_plan_mode → plan_ready(含 fallback markdown)。

    依据:tests/rust/serve_ops.sh:197-265
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "enter_plan_mode", "task": "probe task"})
        pr = first_event(evs, "plan_request")
        assert pr is not None, f"未见 plan_request:{msg_types(evs)}"
        assert pr["msg"].get("task") == "probe task", f"plan_request.task={pr['msg']}"

        evs = s.submit({"type": "exit_plan_mode"})
        ready = first_event(evs, "plan_ready")
        assert ready is not None, f"未见 plan_ready:{msg_types(evs)}"
        assert ready["msg"].get("plan_id"), f"plan_ready 缺 plan_id:{ready['msg']}"
        assert ready["msg"].get("markdown"), "plan_ready 应带 fallback markdown"


def test_A8b_plan审批_工具路径全链路(serve_session, mock_script) -> None:
    """PlanWrite → ExitPlanMode → plan_approval:auto_mode → 切权限模式 + plan_approved。

    与 A8(op 路径)的区别:op 路径只产出 `plan_ready`(无审批 waiter);
    **工具路径**才有完整的审批闭环。依据:submission_loop.rs:1432-1444
    (Exit 方向补发 `plan_approved`)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"PlanWrite","args":{"path":"p1.md","content":"# plan\\n- s1"}}',
            '{"type":"tool_call","name":"ExitPlanMode","args":{}}',
            '{"type":"text","text":"built it"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 注意:工具路径下 ExitPlanMode 会**阻塞 turn** 直到收到 plan_approval
        # —— 审批必须作为另一条 submission 在 turn 在飞时回执(与 bash 套件同款)。
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "draft the plan"}]}
        )
        state: dict[str, Any] = {"approved": False}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "plan_ready" and not state["approved"]:
                state["approved"] = True
                s.send(
                    {
                        "type": "plan_approval",
                        "id": m["plan_id"],
                        "choice": "auto_mode",
                    }
                )

        evs = s.collect(sid, timeout=60.0, on_event=_on_event)
        draft = first_event(evs, "plan_draft_updated")
        assert draft is not None, f"未见 plan_draft_updated:{msg_types(evs)}"
        assert draft["msg"].get("markdown"), f"plan_draft_updated 缺 markdown:{draft['msg']}"
        ready = first_event(evs, "plan_ready")
        assert ready is not None, f"未见 plan_ready:{msg_types(evs)}"
        assert state["approved"], "未回 plan_approval"
        # 审批落地:权限模式切 accept_edits + 发 plan_approved。
        # 注意:工具路径下这两个事件落在**turn 自己的通道**上(sub_id = turn 的
        # submission id),不是全局事件 —— 与 `set_permission_mode` op 路径
        # (走 apply_permission_mode_change,id 为空)不同。
        pmc = first_event(evs, "permission_mode_changed")
        assert pmc is not None, f"未见 permission_mode_changed:{msg_types(evs)}"
        assert pmc["msg"].get("to") == "accept_edits", f"权限模式未切:{pmc['msg']}"
        approved = first_event(evs, "plan_approved")
        assert approved is not None, f"Exit 方向审批应发 plan_approved:{msg_types(evs)}"
        assert approved["msg"].get("plan_id") == ready["msg"]["plan_id"], (
            f"plan_approved 的 plan_id 应与 plan_ready 一致:{approved['msg']}"
        )


def test_A9_四种孤儿回执_不崩且会话存活(serve_session, mock_script) -> None:
    """无 pending waiter 的回执:不产生 error 事件、迭代器收尾、会话存活。

    依据:submission_loop.rs:1002-1013(查不到 waiter → 静默丢弃)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"alive-1"}',
            '{"type":"text","text":"alive-2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        for op in (
            {"type": "tool_approval", "id": "nonexistent", "decision": "approve"},
            {"type": "hook_approval", "id": "nonexistent", "decision": "approve"},
            {
                "type": "ask_user_question_response",
                "id": "nonexistent",
                "answers": {"answers": []},
            },
            {"type": "ask_user_input_response", "id": "nonexistent", "text": "x"},
        ):
            evs = s.submit(op)
            assert msg_types(evs) == ["submission_closed"], f"{op['type']} 未收尾:{msg_types(evs)}"
        # 会话存活:下一轮 prompt 正常(取第一条 mock 脚本回复)。
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]})
        text = "".join(
            e["msg"]["delta"] for e in turn if e["msg"]["type"] == "agent_message_delta"
        )
        assert text == "alive-1", f"会话未存活:{text!r}"
        errs = [e for e in s.global_events() if e["msg"]["type"] == "error"]
        assert not errs, f"孤儿回执不应产生 error:{errs}"


def test_A10_steer_中途转向(serve_session, mock_script, sandbox) -> None:
    """steer(now):转向消息进入上下文(落盘为 user message),turn 仍正常收尾。

    依据:op.rs:167-171、graph/nodes/pre_loop.rs:477-556(注入后落盘)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"after-steer"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {
                "type": "steer",
                "priority": "now",
                "items": [{"type": "text", "text": "STEER-TEXT-XYZ"}],
            }
        )
        assert "submission_closed" in msg_types(evs), f"steer 未收尾:{msg_types(evs)}"
        # 下一轮:转向消息应已进入上下文(落盘为 user message 记录)。
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "继续"}]})
        import time

        time.sleep(1.0)
        recs = rollout_records(sandbox.home)
    steer_hits = [
        r
        for r in recs
        if r.get("type") == "message"
        and "STEER-TEXT-XYZ" in json.dumps(r, ensure_ascii=False)
    ]
    assert steer_hits, f"rollout 未见转向消息:{[r.get('type') for r in recs]}"


def test_A11_query_subagents_状态快照(serve_session, mock_script) -> None:
    """query_subagents → `subagent_status`(children 数组)。

    依据:submission_loop.rs:986-1001。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "query_subagents"})
    m = first_event(evs, "subagent_status")
    assert m is not None, f"未见 subagent_status:{msg_types(evs)}"
    assert isinstance(m["msg"].get("children"), list), f"children 应为数组:{m['msg']}"


def test_A12_prompt_command_提示词层管理(serve_session, mock_script) -> None:
    """prompt_command(list/switch) → `prompt_list` / `prompt_switched`。

    依据:op_prompt.rs:8-19(PromptAction 是 serde tag=type 的对象),
    submission_loop.rs:1195-1237。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "prompt_command", "action": {"type": "list"}})
        m = first_event(evs, "prompt_list")
        assert m is not None, f"未见 prompt_list:{msg_types(evs)}"
        for f in ("available", "current_spec", "active_layers"):
            assert f in m["msg"], f"prompt_list 缺 {f}:{m['msg']}"

        evs = s.submit({"type": "prompt_command", "action": {"type": "switch", "layer": None}})
        m = first_event(evs, "prompt_switched")
        assert m is not None, f"未见 prompt_switched:{msg_types(evs)}"
        assert "active_layers" in m["msg"], f"prompt_switched 缺 active_layers:{m['msg']}"
