"""能力覆盖层 · CLI 能力面:子命令的**行为**断言(参数矩阵仍在 tests/rust/*.sh)。

覆盖:
- C1 `serve` 的三个恢复入口(参数被接受 / 非法 id 报错明确);
- C2 `exec` 的 JSONL 输出契约(每行合法 JSON + 终态事件);
- C3 `session` 子命令(ls 能列出会话;show 对不存在的 id 报错);
- C4 `doctor` / `version` 的输出契约。

运行:tests/run_all.sh functional,或
  REFLECT_BIN=... python3 -m pytest tests/functional/test_cli.py -v
"""

from __future__ import annotations

import json
import time

import pytest

from wire import require_binary, rollout_files, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")


def test_C1_serve_恢复入口(sandbox) -> None:
    """`serve` 接受 --resume / -c / -r;非法 thread id 报错信息明确。"""
    require_binary()
    help_out = run_cli(sandbox, "serve", "--help", timeout=30)
    assert help_out.returncode == 0, f"serve --help 退出码={help_out.returncode}"
    for flag in ("--resume", "--continue-last", "--resume-by"):
        assert flag in help_out.stdout, f"serve --help 缺 {flag}:{help_out.stdout[:400]}"

    # 非法 thread id:provider 检查在前,故给 mock provider 才能走到 id 解析。
    bad = run_cli(
        sandbox,
        "serve",
        "--resume",
        "not-a-uuid",
        env={"REFLECT_MODEL": "mock"},
        timeout=30,
    )
    assert bad.returncode != 0, "非法 thread id 应报错"
    assert "invalid thread id" in (bad.stdout + bad.stderr), (
        f"报错文案异常:{bad.stdout + bad.stderr!r}"
    )


def test_C2_exec_输出JSONL(sandbox) -> None:
    """`exec` 的 stdout 每行是合法 JSON,且含 turn 事件流。"""
    require_binary()
    proc = run_cli(
        sandbox,
        "exec",
        "hello",
        env={"REFLECT_MODEL": "mock"},
        timeout=60,
    )
    assert proc.returncode == 0, f"exec 退出码={proc.returncode}:{proc.stderr[:500]}"
    lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
    assert lines, f"exec 无输出:{proc.stdout!r}"
    events = []
    for ln in lines:
        try:
            events.append(json.loads(ln))
        except json.JSONDecodeError as exc:
            pytest.fail(f"exec 输出非 JSON 行:{ln!r}({exc})")
    types = [e.get("msg", {}).get("type") for e in events]
    assert "turn_started" in types, f"缺 turn_started:{types}"
    assert "turn_complete" in types, f"缺 turn_complete:{types}"


def test_C3_session_子命令(sandbox, serve_session, mock_script) -> None:
    """`session ls` 列出刚写入的会话;`session show` 对不存在的 id 报错非 0。

    索引里的 session_id 取自 SessionMeta 记录(与文件名的 thread id 不同)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, "无会话文件"
    import json as _json

    session_id = None
    for ln in files[-1].read_text().splitlines():
        if ln.strip():
            rec = _json.loads(ln)
            if rec.get("type") == "session_meta":
                session_id = rec.get("session_id")
    assert session_id, "无 session_meta / session_id"

    listed = run_cli(sandbox, "session", "ls", env={"REFLECT_MODEL": "mock"}, timeout=30)
    assert listed.returncode == 0, f"session ls 退出码={listed.returncode}:{listed.stderr}"
    assert session_id in listed.stdout, f"session ls 未列出:{listed.stdout!r}"

    missing = run_cli(
        sandbox, "session", "show", "00000000-0000-0000-0000-000000000000", timeout=30
    )
    assert missing.returncode != 0, "session show 不存在的 id 应报错"


def test_C4_doctor与version(sandbox) -> None:
    """doctor / version 的输出契约。"""
    require_binary()
    ver = run_cli(sandbox, "version", timeout=30)
    assert ver.returncode == 0, f"version 退出码={ver.returncode}:{ver.stderr}"
    assert "reflect" in ver.stdout.lower(), f"version 输出异常:{ver.stdout!r}"

    doc = run_cli(
        sandbox, "doctor", env={"REFLECT_MODEL": "mock"}, timeout=60
    )
    assert doc.returncode == 0, f"doctor 退出码={doc.returncode}:{doc.stderr[:500]}"
    assert doc.stdout.strip(), "doctor 无输出"
