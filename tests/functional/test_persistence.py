"""能力覆盖层 · 持久化面:会话 JSONL 的记录集、轨迹与物理截断。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- P1 rollout 记录集(session_meta 首行 + message);
- P2 权限模式轨迹(from/to/at);
- P3 rewind 物理截断(记录被丢弃 + `.rewind-<ts>.bak` 备份);
- P4 会话索引(`zhuye session ls` 能列出该会话)。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_persistence.py -v
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    require_binary,
    rollout_files,
    rollout_records,
    run_cli,
)

pytestmark = pytest.mark.usefixtures("sandbox")


def test_P1_rollout记录集(serve_session, mock_script, sandbox) -> None:
    """会话 JSONL:首行 `session_meta` + 每轮的 user/assistant `message`。

    依据:crates/resources/zhuye-rollout/src/writer.rs(首行 = SessionMeta
    是索引契约)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"hello"}',
            '{"type":"text","text":"world"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, f"未落盘会话文件:{sandbox.home}/.zhuye/sessions"
    lines = [json_line for json_line in files[-1].read_text().splitlines() if json_line.strip()]
    recs = [__import__("json").loads(json_line) for json_line in lines]
    assert recs[0].get("type") == "session_meta", f"首行应为 session_meta:{recs[0]}"
    assert recs[0].get("session_id"), "session_meta 缺 session_id"
    msgs = [r for r in recs if r.get("type") == "message"]
    assert len(msgs) >= 4, f"两轮应有 ≥4 条 message:{len(msgs)}"
    roles = {r.get("role") for r in msgs}
    assert {"user", "assistant"} <= roles, f"缺角色:{roles}"


def test_P2_权限模式轨迹落盘(serve_session, mock_script, sandbox) -> None:
    """权限模式切换的 from→to 轨迹 + at 时间戳落盘。

    依据:tests/COVERAGE.md §4(SDK 审批面 · rollout 持久化)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit({"type": "set_permission_mode", "mode": "plan"})
        s.submit({"type": "cycle_permission_mode"})
    time.sleep(0.8)
    recs = rollout_records(sandbox.home)
    pmc = [r for r in recs if r.get("type") == "permission_mode_changed"]
    assert pmc, f"无 permission_mode_changed 记录:{[r.get('type') for r in recs]}"
    assert [(r.get("from"), r.get("to")) for r in pmc] == [
        ("auto", "plan"),
        ("plan", "auto"),
    ], f"pmc 轨迹不符:{pmc}"
    assert all(r.get("at") for r in pmc), f"pmc 记录缺 at:{pmc}"


def test_P3_rewind_物理截断并备份(serve_session, mock_script, sandbox) -> None:
    """rewind → 被回退的 turn 记录从 JSONL 中消失,并留下 `.rewind-<ts>.bak` 备份。

    依据:crates/resources/zhuye-rollout/src/writer.rs:254(备份)、
    :309-334(按 cut 点物理截断)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"one"}',
            '{"type":"text","text":"two"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        time.sleep(0.8)
        before = len(rollout_records(sandbox.home))
        assert before >= 4, f"两轮记录数不足:{before}"
        # rewind 必须在**同一会话**里做:换进程会写新的会话文件,截断的不是这份。
        s.submit({"type": "rewind"}, timeout=DRAIN_TIMEOUT)
    time.sleep(0.8)
    after = len(rollout_records(sandbox.home))
    assert after < before, f"rewind 应物理截断记录({before} → {after})"
    baks = list((sandbox.home / ".zhuye/sessions").glob("*/*/*/*.bak"))
    assert baks, "rewind 应留下 .bak 备份"


def test_P4_会话索引可列出(serve_session, mock_script, sandbox) -> None:
    """`zhuye session ls` 能列出刚写入的会话(首行 = SessionMeta 的索引契约)。

    注意:索引里的 `session_id` 取自 **SessionMeta 记录**,与**文件名**里的
    thread id 不是同一个 uuid(exec/serve 路径未注入 `with_session_id`)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, "无会话文件"
    meta = [
        r for r in rollout_records(sandbox.home) if r.get("type") == "session_meta"
    ]
    assert meta, "无 session_meta 记录"
    session_id = meta[-1]["session_id"]

    proc = run_cli(sandbox, "session", "ls", env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session ls 退出码={proc.returncode}:{proc.stderr}"
    assert session_id in proc.stdout, (
        f"session ls 未列出该会话:{session_id!r}\nstdout={proc.stdout!r}"
    )
