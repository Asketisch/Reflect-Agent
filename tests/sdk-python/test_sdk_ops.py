"""reflect-agent Python SDK 深度用例:submit_op 全 Op 面 + 审批 + 持久化。

与 test_sdk_real_bin.py(核心协议回路)互补,本文件驱动**非 turn 操作**
的完整使用路径 —— 这些路径此前没有终态事件,SDK 迭代器会永久挂起,
v1.3 起由 serve 在 per-turn 通道排空后发 `submission_closed` 收尾:

  1. 非 turn ops 逐条 submit_op:compact / set_effort / rewind /
     set_permission_mode / cycle_permission_mode / enter_goal_mode /
     exit_goal_mode / enter_plan_mode / exit_plan_mode / plan_approval,
     每条迭代器确定性收尾且末尾是 `submission_closed`;
  2. 权限模式切换的全局事件(`permission_mode_changed`,id="")不进
     按 id 路由的迭代器 → 改断言 rollout JSONL 里的持久化记录
     (`permission_mode_changed` record,含 from/to 轨迹);
  3. MCP 工具审批全链路(REFLECT_APPROVALS=1):approval_request →
     `approve(id, "approve")` → 工具真执行 → tool_call_end → turn_complete;
  4. 孤儿审批回执(无 pending waiter)不崩、会话存活。

运行:tests/sdk-python/run.sh,或
  REFLECT_BIN=... python3 -m pytest tests/sdk-python -v
"""

from __future__ import annotations

import glob
import json
import os
import threading
import time
from pathlib import Path
from typing import Any, Iterator

import pytest

from reflect import ReflectAgent, ReflectAgentOptions

ROOT = Path(__file__).resolve().parents[2]
REFLECT_BIN = Path(
    os.environ.get("REFLECT_BIN", str(ROOT / "target/release/reflect"))
)
MOCK_MCP = ROOT / "target/debug/mock_mcp_server"

# 迭代器墙钟预算:submission_closed 缺失(回归)时绝不挂死整个套件。
DRAIN_TIMEOUT = 30.0


@pytest.fixture()
def env_home() -> Iterator[Path]:
    """隔离 HOME:预写最小 config.toml(关全部内置 hook)。

    与 test_sdk_real_bin.py 同名 fixture 独立一份:测试文件自包含,
    run.sh 逐文件驱动时不依赖 conftest。
    """
    import tempfile

    home = Path(tempfile.mkdtemp(prefix="reflect-pytest-ops-home."))
    reflect_dir = home / ".reflect"
    reflect_dir.mkdir(parents=True)
    (reflect_dir / "config.toml").write_text(
        "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n"
    )
    old_home = os.environ.get("HOME")
    os.environ["HOME"] = str(home)
    yield home
    os.environ.pop("REFLECT_MODEL", None)
    os.environ.pop("REFLECT_MOCK_SCRIPT", None)
    os.environ.pop("REFLECT_APPROVALS", None)
    if old_home is not None:
        os.environ["HOME"] = old_home
    if not os.environ.get("REFLECT_KEEP"):
        import shutil

        shutil.rmtree(home, ignore_errors=True)


def _options(
    script_lines: list[str] | None = None, extra_env: dict[str, str] | None = None
) -> ReflectAgentOptions:
    """指向真二进制 + mock provider 的 spawn 选项;extra_env 透传给子进程。"""
    import tempfile

    tmp = Path(tempfile.mkdtemp(prefix="reflect-pytest-ops-script."))
    env: dict[str, str] = {"REFLECT_MODEL": "mock"}
    if script_lines is not None:
        script = tmp / "script.jsonl"
        script.write_text("\n".join(script_lines) + "\n")
        env["REFLECT_MOCK_SCRIPT"] = str(script)
    if extra_env:
        env.update(extra_env)
    return ReflectAgentOptions(
        bin=str(REFLECT_BIN), env=env, handshake_timeout_secs=15
    )


def drain(events_iter: Iterator[dict[str, Any]]) -> list[dict[str, Any]]:
    """在独立线程消费 submit_op 迭代器,带墙钟超时。

    迭代器收尾依赖 `submission_closed`(serve 在 per-turn 通道排空后发);
    若回归导致缺失,join 超时后断言失败而不是拖死整个测试套件。
    """
    events: list[dict[str, Any]] = []
    finished: list[bool] = []

    def _run() -> None:
        for ev in events_iter:
            events.append(ev)
        finished.append(True)

    t = threading.Thread(target=_run, daemon=True)
    t.start()
    t.join(DRAIN_TIMEOUT)
    assert finished, f"{DRAIN_TIMEOUT}s 内迭代器未收尾:events={events!r}"
    return events


def _types(events: list[dict[str, Any]]) -> list[str]:
    return [ev["msg"]["type"] for ev in events]


def _find_rollouts(home: Path) -> list[Path]:
    return sorted(glob.glob(str(home / ".reflect/sessions/*/*/*/*.jsonl")))


def _rollout_records(home: Path) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for f in _find_rollouts(home):
        for line in Path(f).read_text().splitlines():
            if line.strip():
                out.append(json.loads(line))
    return out


# ── 1. 非 turn ops:迭代器确定性收尾 + 关键事件断言 ─────────────────────


def test_submit_op_non_turn_ops(env_home: Path) -> None:
    agent = ReflectAgent.spawn(
        _options(
            [
                '{"type":"text","text":"t1"}',
                '{"type":"text","text":"t2"}',
                '{"type":"text","text":"t3"}',
            ]
        )
    )
    try:
        # 先来一轮真 turn:给 rewind 留可回退对象,同时验证 turn 面。
        turn_events = drain(agent.submit("第一轮"))
        assert "turn_complete" in _types(turn_events)

        # compact → context_compacted 挂本 submission id。
        evs = drain(agent.submit_op({"type": "compact"}))
        ts = _types(evs)
        assert "context_compacted" in ts, f"compact: {ts!r}"
        assert ts[-1] == "submission_closed", f"compact 末尾非收尾: {ts!r}"

        # set_effort → 静默写槽:整条只有收尾标记,无任何事件。
        evs = drain(agent.submit_op({"type": "set_effort", "effort": "high"}))
        assert _types(evs) == ["submission_closed"], f"set_effort 应静默: {_types(evs)!r}"

        # rewind → turn_rewound 挂本 submission id。
        evs = drain(agent.submit_op({"type": "rewind"}))
        ts = _types(evs)
        assert "turn_rewound" in ts, f"rewind: {ts!r}"
        assert ts[-1] == "submission_closed"

        # 权限模式 ops:permission_mode_changed 是全局事件(id=""),
        # 不进按 id 路由的迭代器 → 这里只断言收尾(持久化见下一用例)。
        evs = drain(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
        assert _types(evs) == ["submission_closed"], f"set_pm: {_types(evs)!r}"
        evs = drain(agent.submit_op({"type": "cycle_permission_mode"}))
        assert _types(evs) == ["submission_closed"], f"cycle_pm: {_types(evs)!r}"

        # goal 模式进出:静默,只有收尾标记。
        evs = drain(
            agent.submit_op(
                {"type": "enter_goal_mode", "goal": "probe goal", "token_budget": 100000}
            )
        )
        assert _types(evs) == ["submission_closed"], f"enter_goal: {_types(evs)!r}"
        evs = drain(agent.submit_op({"type": "exit_goal_mode"}))
        assert _types(evs) == ["submission_closed"], f"exit_goal: {_types(evs)!r}"

        # plan 模式 ops:enter → plan_request(本 id);exit → plan_ready;
        # plan_approval(auto_mode)→ 全局 pmc,迭代器只有收尾标记。
        evs = drain(agent.submit_op({"type": "enter_plan_mode", "task": "probe task"}))
        ts = _types(evs)
        assert "plan_request" in ts, f"enter_plan: {ts!r}"
        assert ts[-1] == "submission_closed"
        pr = next(ev["msg"] for ev in evs if ev["msg"]["type"] == "plan_request")
        assert pr.get("task") == "probe task", f"plan_request.task: {pr!r}"

        evs = drain(agent.submit_op({"type": "exit_plan_mode"}))
        ts = _types(evs)
        assert "plan_ready" in ts, f"exit_plan: {ts!r}"
        ready = next(ev["msg"] for ev in evs if ev["msg"]["type"] == "plan_ready")
        plan_id = ready["plan_id"]
        assert ready["markdown"], "plan_ready 应带 fallback markdown"

        evs = drain(
            agent.submit_op(
                {"type": "plan_approval", "id": plan_id, "choice": "auto_mode"}
            )
        )
        assert _types(evs) == ["submission_closed"], f"plan_approval: {_types(evs)!r}"
    finally:
        agent.close()


# ── 2. 权限模式切换 → rollout JSONL 持久化轨迹 ─────────────────────────


def test_permission_mode_persisted_in_rollout(env_home: Path) -> None:
    agent = ReflectAgent.spawn(_options(['{"type":"text","text":"t1"}']))
    try:
        # 一轮真 turn,让 session_meta / message 先落盘。
        drain(agent.submit("持久化探针"))
        drain(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
        drain(agent.submit_op({"type": "cycle_permission_mode"}))
        time.sleep(0.5)  # 落盘是 best-effort spawn,给 writer 一点时间

        recs = _rollout_records(env_home)
        assert any(r.get("type") == "session_meta" for r in recs), f"无 session_meta: {recs!r}"
        assert any(r.get("type") == "message" for r in recs), f"无 message: {recs!r}"

        pmc = [r for r in recs if r.get("type") == "permission_mode_changed"]
        # auto→plan(set)+ plan→auto(cycle)各一条,顺序保持。
        assert [(r["from"], r["to"]) for r in pmc] == [
            ("auto", "plan"),
            ("plan", "auto"),
        ], f"pmc 轨迹: {pmc!r}"
        # 每条 record 自带 at 时间戳(resume/export 依赖)。
        assert all(r.get("at") for r in pmc), f"pmc 缺 at: {pmc!r}"
    finally:
        agent.close()


# ── 3. MCP 工具审批全链路(approval_request → approve → 执行)────────


def test_approve_flow_mcp_tool(env_home: Path) -> None:
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p reflect-mcp)")
    # MCP server 配置:echo 工具 required_permission=Prompt → Auto 模式下
    # 必经审批 gate(reflect-permissions:Auto 模式 auto_approves_tool=False)。
    cfg = env_home / ".reflect/config.toml"
    cfg.write_text(
        cfg.read_text()
        + f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
    )
    agent = ReflectAgent.spawn(
        _options(
            [
                '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
                '{"type":"text","text":"approved-done"}',
            ],
            extra_env={"REFLECT_APPROVALS": "1"},
        )
    )
    try:
        # MCP 注册是异步的:握手 + list_tools 完成前调用会报 unknown tool。
        time.sleep(2)
        events: list[dict[str, Any]] = []
        approval_seen: dict[str, Any] = {}

        def _run() -> None:
            for ev in agent.submit("调 echo 工具"):
                events.append(ev)
                m = ev["msg"]
                if m.get("type") == "approval_request":
                    approval_seen.update(m)
                    agent.approve(m["request_id"], "approve")
                elif m.get("type") == "turn_complete":
                    break

        t = threading.Thread(target=_run, daemon=True)
        t.start()
        t.join(DRAIN_TIMEOUT)
        ts = _types(events)
        assert "request_id" in approval_seen, f"未见 approval_request: {ts!r}"
        assert approval_seen["kind"]["type"] == "tool"
        assert approval_seen["kind"]["tool_name"] == "mcp__fsx__echo"
        # approve 回执接通 oneshot 后 turn 照常收尾(join 未超时 + 见终态)。
        assert "turn_complete" in ts, f"审批后 turn 未收尾: {ts!r}"
        # 放行后 MCP 工具真执行:tool_call_end 带回显文本 "probe"。
        tce = [ev["msg"] for ev in events if ev["msg"]["type"] == "tool_call_end"]
        assert tce, f"未见 tool_call_end: {ts!r}"
        assert any(
            ev.get("is_error") is False and "probe" in json.dumps(ev, ensure_ascii=False)
            for ev in tce
        ), f"tool_call_end 未含回显: {tce!r}"
    finally:
        agent.close()


# ── 4. 孤儿审批回执:无 pending waiter 不崩、会话存活 ───────────────────


def test_orphan_approval_session_survives(env_home: Path) -> None:
    agent = ReflectAgent.spawn(_options(['{"type":"text","text":"alive"}']))
    try:
        # 无对应 approval_request 的回执:core 侧查不到 waiter,静默丢弃。
        agent.approve("nonexistent-id", "approve")
        time.sleep(0.5)
        # 会话仍健康:下一轮 prompt 正常回流。
        assert agent.prompt("还在吗") == "alive"
    finally:
        agent.close()
