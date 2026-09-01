"""reflect-agent Python SDK 端到端测试(驱动真实 `reflect serve` 二进制)。

与 sdks/python/tests/test_protocol.py(Node mock serve)互补:本文件
spawn **真二进制**,用 mock provider(`REFLECT_MODEL=mock`)+ mock 脚本
驱动确定性回合,验证 SDK 的完整协议回路:

  1. spawn → session_configured 握手(model=mock/mock-1);
  2. prompt 单轮 → mock 脚本文本回流;
  3. 多轮 prompt 上下文保持(脚本逐行消耗,两轮回复不同);
  4. register_tool → LLM 发起远程工具调用 → SDK 本地执行 → 回执 →
     turn_complete;
  5. interrupt 打断当前 turn(serve 侧 emit turn_aborted);
  6. close 幂等优雅退出;
  7. from_popen 对坏二进制抛错(handshake 超时/EOF)。

环境变量:
  REFLECT_BIN    —— reflect 二进制路径(默认 target/release/reflect)
  REFLECT_KEEP   —— =1 时保留临时目录便于排查

运行:tests/sdk-python/run.sh,或
  REFLECT_BIN=... python3 -m pytest tests/sdk-python -v
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Iterator

import pytest

from reflect import ContentBlock, ReflectAgent, ReflectAgentOptions, ToolOutput

ROOT = Path(__file__).resolve().parents[2]
REFLECT_BIN = Path(
    os.environ.get("REFLECT_BIN", str(ROOT / "target/release/reflect"))
)


@pytest.fixture()
def env_home() -> Iterator[Path]:
    """隔离 HOME:预写关闭全部内置 hook 的最小 config.toml。

    exec/serve 默认启用 VerificationHook(Stop 时跑 `cargo test`)等
    重 hook,单轮会拖到分钟级;SDK 场景只需协议语义,统一关掉。
    config.toml 必须存在,否则 ConfigWatcher 直接报错退出。
    """
    home = Path(tempfile.mkdtemp(prefix="reflect-pytest-home."))
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
    if old_home is not None:
        os.environ["HOME"] = old_home
    if not os.environ.get("REFLECT_KEEP"):
        import shutil

        shutil.rmtree(home, ignore_errors=True)


def _options(script_lines: list[str] | None = None) -> ReflectAgentOptions:
    """构造指向真二进制 + mock provider 的 spawn 选项。

    script_lines 非 None 时写临时 mock 脚本并挂 REFLECT_MOCK_SCRIPT。
    """
    tmp = Path(tempfile.mkdtemp(prefix="reflect-pytest-script."))
    if script_lines is not None:
        script = tmp / "script.jsonl"
        script.write_text("\n".join(script_lines) + "\n")
        env = {"REFLECT_MODEL": "mock", "REFLECT_MOCK_SCRIPT": str(script)}
    else:
        env = {"REFLECT_MODEL": "mock"}
    return ReflectAgentOptions(
        bin=str(REFLECT_BIN),
        env=env,
        handshake_timeout_secs=15,
    )


# ── 1. 握手 ────────────────────────────────────────────────────────────────


def test_spawn_handshake(env_home: Path) -> None:
    # spawn 内部等待 session_configured 才返回;超时/失败会直接抛
    # RuntimeError,因此「spawn 成功返回」即握手成功的充分证据。
    agent = ReflectAgent.spawn(_options())
    agent.close()


# ── 2. 单轮 prompt:mock 脚本文本逐字回流 ──────────────────────────────────


def test_prompt_single_turn(env_home: Path) -> None:
    agent = ReflectAgent.spawn(
        _options(['{"type":"text","text":"py-e2e-reply"}'])
    )
    try:
        assert agent.prompt("第一轮") == "py-e2e-reply"
    finally:
        agent.close()


# ── 3. 多轮:上下文保持(mock 脚本按调用顺序逐行消耗)────────────────────


def test_prompt_multi_turn_context(env_home: Path) -> None:
    agent = ReflectAgent.spawn(
        _options(
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


# ── 4. 自定义工具:注册 → LLM 调用 → 客户端执行 → 回执 ────────────────────


def test_register_tool_roundtrip(env_home: Path) -> None:
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any]) -> ToolOutput:
        calls.append(args)
        return ToolOutput(
            content=[ContentBlock(type="text", text=f"echo:{args['q']}")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent = ReflectAgent.spawn(
        _options(
            [
                '{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}',
                '{"type":"text","text":"tool-done"}',
            ]
        )
    )
    try:
        agent.register_tool("sdk_echo", "echo back", {"type": "object"}, handler)
        saw_complete = False
        for ev in agent.submit("调工具"):
            if ev["msg"]["type"] == "turn_complete":
                saw_complete = True
                break
        assert calls == [{"q": "ping"}], f"calls={calls!r}"
        assert saw_complete, "未见 turn_complete"
    finally:
        agent.close()


# ── 5. interrupt:打断处于远程工具等待中的 turn(确定性)─────────────────
#
# wire 语义(实测):interrupt 的 turn_aborted 事件挂在 **interrupt 这条
# submission 自己的 id** 上,不路由到原 turn 的迭代器。因此用 submit_op
# 指定 id 提交 interrupt,并在该迭代器上断言;原 turn 的迭代器随后正常
# 以 turn_complete 终止(interrupt 不会硬取消停在远程工具等待上的 turn,
# 回执到达后照常收尾)。


def test_interrupt_aborts_turn(env_home: Path) -> None:
    import threading

    handler_started = threading.Event()
    release_handler = threading.Event()

    def handler(args: dict[str, Any]) -> ToolOutput:
        handler_started.set()
        release_handler.wait(timeout=10)
        return _text_output("slow result")

    agent = ReflectAgent.spawn(
        _options(
            [
                '{"type":"tool_call","name":"slow_tool","args":{}}',
                '{"type":"text","text":"after"}',
            ]
        )
    )
    try:
        agent.register_tool("slow_tool", "block until released", {"type": "object"}, handler)
        turn_events = agent.submit("调慢工具")
        # 等 handler 进入等待(即 turn 阻塞在远程工具上)再打断
        assert handler_started.wait(timeout=10), "handler 未被调用"
        intr_events = agent.submit_op({"type": "interrupt"}, submission_id="intr-1")
        intr_types = [ev["msg"]["type"] for ev in intr_events]
        release_handler.set()
        assert "turn_aborted" in intr_types, f"intr events={intr_types!r}"
        # 回执释放后原 turn 正常收尾
        turn_types = [ev["msg"]["type"] for ev in turn_events]
        assert "turn_complete" in turn_types, f"turn events={turn_types!r}"
    finally:
        release_handler.set()
        agent.close()


# ── 6. close 幂等 ──────────────────────────────────────────────────────────


def test_close_idempotent(env_home: Path) -> None:
    agent = ReflectAgent.spawn(_options())
    agent.close()
    agent.close()  # 第二次不应抛错


# ── 7. 坏二进制:from_popen 路径报错而非挂死 ───────────────────────────────


def test_from_popen_bad_binary(env_home: Path) -> None:
    # from_popen 第二个参数是 handshake 超时秒数(float),非 options
    with pytest.raises(RuntimeError):
        ReflectAgent.from_popen(
            subprocess.Popen(
                ["/bin/sh", "-c", "echo not-jsonl && sleep 5"],
                stdout=subprocess.PIPE,
            ),
            3.0,
        )
