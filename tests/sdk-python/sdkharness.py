"""Python SDK 绑定层的可复用助手(唯一命名模块,避免与其它 conftest 同名冲突)。"""

from __future__ import annotations

import json
import os
import tempfile
import threading
from pathlib import Path
from typing import Any, Iterator

# 仓库根:tests/sdk-python/sdkharness.py → 上溯两级。
ROOT = Path(__file__).resolve().parents[2]
# 被测二进制:默认 release 产物(与 tests/lib.sh 的约定一致)。
ZHUYE_BIN = Path(
    os.environ.get("ZHUYE_BIN", str(ROOT / "target/release/zhuye"))
)
# 迭代器墙钟预算:终态事件缺失(回归)时断言失败,绝不挂死整个套件。
DRAIN_TIMEOUT = 30.0


def make_options(
    script_lines: list[str] | None = None,
    extra_env: dict[str, str] | None = None,
    handshake_timeout_secs: float = 15.0,
):
    """构造指向真二进制 + mock provider 的 spawn 选项。

    `script_lines` 非 None 时写临时 mock 脚本并挂 `ZHUYE_MOCK_SCRIPT`。
    """
    from zhuye import ZhuyeAgentOptions

    tmp = Path(tempfile.mkdtemp(prefix="zhuye-pytest-script."))
    if script_lines is not None:
        script = tmp / "script.jsonl"
        script.write_text("\n".join(script_lines) + "\n")
        env = {"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}
    else:
        env = {"ZHUYE_MODEL": "mock"}
    if extra_env:
        env.update(extra_env)
    return ZhuyeAgentOptions(
        bin=str(ZHUYE_BIN),
        env=env,
        handshake_timeout_secs=handshake_timeout_secs,
    )


def drain(events_iter: Iterator[dict[str, Any]]) -> list[dict[str, Any]]:
    """在独立线程消费迭代器,带墙钟超时。

    迭代器收尾依赖 `turn_complete` / `turn_aborted` / `submission_closed`;
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


def msg_types(events: list[dict[str, Any]]) -> list[str]:
    """事件流的消息类型序列。"""
    return [ev["msg"]["type"] for ev in events]


def rollout_records(home: Path) -> list[dict[str, Any]]:
    """读隔离 HOME 下的会话记录。"""
    out: list[dict[str, Any]] = []
    for f in sorted((home / ".zhuye/sessions").glob("*/*/*/*.jsonl")):
        for line in f.read_text().splitlines():
            if line.strip():
                out.append(json.loads(line))
    return out
