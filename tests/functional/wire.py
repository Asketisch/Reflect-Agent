"""能力覆盖层的原始 wire 测试设施(不依赖任何 SDK)。

**为什么要自己起子进程**:能力覆盖层测的是 **Rust 代码在真产物上的行为**,
如果通过 Python / TS SDK 去驱动,SDK 自身的 bug 会把 Rust 的 bug 遮住
(实例:Python SDK 的 `close()` 从不发出 shutdown op —— 见 COVERAGE.md
「已知缺陷」D1)。因此本层只用 `subprocess` + `json` 打原始 JSONL wire,
**不 import `reflect`**。

共享设施放在本模块(唯一命名)而不是 `conftest.py`:两个测试层各有一个
conftest.py,`from conftest import ...` 在一次性收集整个 tests/ 时会按
sys.path 顺序解析到错误的那一份。
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable, Iterator


import json
import os
import shutil
import subprocess
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

# 仓库根:tests/functional/wire.py → 上溯两级。
ROOT = Path(__file__).resolve().parents[2]
# 被测二进制:默认 release 产物(与 tests/lib.sh 的约定一致)。
REFLECT_BIN = Path(
    os.environ.get("REFLECT_BIN", str(ROOT / "target/release/reflect"))
)
# MCP 审批用例需要的 mock stdio server(由 `cargo build -p reflect-mcp` 产出)。
MOCK_MCP = ROOT / "target/debug/mock_mcp_server"

# 终结一条 submission 事件流的事件类型(见 sdks/PROTOCOL.md §3)。
TERMINAL_TYPES = frozenset(
    {"turn_complete", "turn_aborted", "shutdown_complete", "submission_closed"}
)
# 墙钟预算:事件流缺终态(回归)时断言失败,绝不挂死整个套件。
DRAIN_TIMEOUT = 30.0


# ── 沙箱 ─────────────────────────────────────────────────────────────────


@dataclass
class Sandbox:
    """隔离的 HOME + cwd;子进程 env 由此派生,不污染宿主进程环境。"""

    home: Path
    cwd: Path

    def env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        """派生子进程 env:HOME 指向隔离目录,其余继承当前进程。"""
        env = dict(os.environ)
        env["HOME"] = str(self.home)
        if extra:
            env.update(extra)
        return env

    def write_config(self, extra: str = "") -> None:
        """在隔离 HOME 的 config.toml 后追加配置(默认已关闭全部内置 hook)。"""
        cfg = self.home / ".reflect/config.toml"
        cfg.write_text(cfg.read_text() + extra)

    def mock_env(self, script: Path | None = None, **extra: str) -> dict[str, str]:
        """派生 mock provider 环境:`REFLECT_MODEL=mock` + 可选脚本。"""
        env: dict[str, str] = {"REFLECT_MODEL": "mock"}
        if script is not None:
            env["REFLECT_MOCK_SCRIPT"] = str(script)
        env.update(extra)
        return env


# ── serve 会话 ───────────────────────────────────────────────────────────


class ServeSession:
    """`reflect serve` 子进程会话:逐行读写 JSONL。

    线程模型:主线程写 stdin / 收集事件;**reader 线程**独占读 stdout,
    逐行解析后按 submission id 分桶并唤醒等待者。
    """

    def __init__(
        self,
        sandbox: Sandbox,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
        timeout: float = DRAIN_TIMEOUT,
    ) -> None:
        self.sandbox = sandbox
        self.args = list(args or [])
        self.timeout = timeout
        self.stderr_path = sandbox.cwd / "serve.stderr"
        self._events: list[dict[str, Any]] = []
        self._by_sub: dict[str, list[dict[str, Any]]] = {}
        self._globals: list[dict[str, Any]] = []
        self._cond = threading.Condition()
        self._write_lock = threading.Lock()
        self._closed = False
        cmd = [str(REFLECT_BIN), "serve", *self.args]
        self._proc = subprocess.Popen(
            cmd,
            cwd=str(sandbox.cwd),
            env=sandbox.env(env),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_path.open("wb"),
            text=True,
            bufsize=1,
        )
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    # ── 读侧 ───────────────────────────────────────────────────────────

    def _read_loop(self) -> None:
        assert self._proc.stdout is not None
        for raw in self._proc.stdout:
            line = raw.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            with self._cond:
                self._events.append(obj)
                ev_id = obj.get("id", "")
                self._by_sub.setdefault(ev_id, []).append(obj)
                if ev_id == "":
                    self._globals.append(obj)
                self._cond.notify_all()
        # EOF:唤醒所有等待者(进程死了也要让用例失败,而不是挂死)。
        with self._cond:
            self._cond.notify_all()

    # ── 写侧 ───────────────────────────────────────────────────────────

    def write_line(self, line: str) -> None:
        """写任意一行到 stdin(用于坏行 / 未知 op 等容错场景)。"""
        with self._write_lock:
            if self._proc.stdin is None:
                raise RuntimeError("serve stdin 已关闭")
            self._proc.stdin.write(line if line.endswith("\n") else line + "\n")
            self._proc.stdin.flush()

    def send(self, op: dict[str, Any], sub_id: str | None = None) -> str:
        """发一条 submission,立即返回其 id(不等待)。"""
        sub_id = sub_id or str(uuid.uuid4())
        self.write_line(json.dumps({"id": sub_id, "op": op}, ensure_ascii=False))
        return sub_id

    def collect(
        self,
        sub_id: str,
        timeout: float | None = None,
        on_event: Callable[[dict[str, Any]], None] | None = None,
    ) -> list[dict[str, Any]]:
        """收集某条 submission 的事件流,直到收到终态事件。

        `on_event` 在每个新事件到达时同步回调 —— 用于审批 / 远程工具
        这类"事件驱动回执"的交互(回执必须在 turn 结束前写回)。
        """
        deadline = time.monotonic() + (timeout if timeout is not None else self.timeout)
        seen = 0
        while True:
            with self._cond:
                evs = list(self._by_sub.get(sub_id, ()))
                if len(evs) > seen:
                    new = evs[seen:]
                    seen = len(evs)
                else:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        got = [e.get("msg", {}).get("type") for e in evs]
                        raise AssertionError(
                            f"{timeout or self.timeout}s 内未收到终态事件:"
                            f"submission={sub_id} 已见={got}"
                        )
                    self._cond.wait(min(remaining, 0.2))
                    continue
            for ev in new:
                if on_event is not None:
                    on_event(ev)
                if ev.get("msg", {}).get("type") in TERMINAL_TYPES:
                    with self._cond:
                        return list(self._by_sub.get(sub_id, ()))

    def submit(
        self,
        op: dict[str, Any],
        timeout: float | None = None,
        on_event: Callable[[dict[str, Any]], None] | None = None,
    ) -> list[dict[str, Any]]:
        """发一条 submission 并等它收尾,返回该 submission 的事件流。"""
        sub_id = self.send(op)
        return self.collect(sub_id, timeout=timeout, on_event=on_event)

    # ── 观察 ───────────────────────────────────────────────────────────

    def all_events(self) -> list[dict[str, Any]]:
        with self._cond:
            return list(self._events)

    def global_events(self) -> list[dict[str, Any]]:
        """全局事件(`id==""`):握手 / 权限模式 / error / 远程工具请求等。"""
        with self._cond:
            return list(self._globals)

    def wait_global(
        self,
        predicate: Callable[[dict[str, Any]], bool],
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """等一个满足条件的全局事件,超时抛断言。"""
        deadline = time.monotonic() + (timeout if timeout is not None else self.timeout)
        while True:
            with self._cond:
                for ev in self._globals:
                    if predicate(ev):
                        return ev
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    seen = [e.get("msg", {}).get("type") for e in self._globals]
                    raise AssertionError(
                        f"{timeout or self.timeout}s 内未等到全局事件:已见={seen}"
                    )
                self._cond.wait(min(remaining, 0.2))

    def wait_event(
        self,
        sub_id: str,
        msg_type: str,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """等某条 submission 里的指定类型事件。"""
        deadline = time.monotonic() + (timeout if timeout is not None else self.timeout)
        while True:
            with self._cond:
                for ev in self._by_sub.get(sub_id, ()):
                    if ev.get("msg", {}).get("type") == msg_type:
                        return ev
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    got = [e.get("msg", {}).get("type") for e in self._by_sub.get(sub_id, ())]
                    raise AssertionError(
                        f"{timeout or self.timeout}s 内未等到 {msg_type}:已见={got}"
                    )
                self._cond.wait(min(remaining, 0.2))

    # ── 生命周期 ───────────────────────────────────────────────────────

    def kill(self) -> None:
        """强杀子进程(模拟崩溃)。"""
        with self._write_lock:
            self._closed = True
        try:
            self._proc.kill()
        except Exception:
            pass

    def close(self, timeout: float = 10.0) -> int:
        """优雅关闭:发 shutdown → 等 `shutdown_complete` → 关 stdin → 等退出。

        返回进程退出码。stdin EOF 等价于 shutdown(见 sdks/PROTOCOL.md §6),
        所以即使 shutdown 没送达,关 stdin 也会让 serve 收尾。
        """
        if self._closed:
            return self._proc.poll() or 0
        self._closed = True
        try:
            self.write_line(
                json.dumps(
                    {"id": str(uuid.uuid4()), "op": {"type": "shutdown"}},
                    ensure_ascii=False,
                )
            )
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                with self._cond:
                    done = any(
                        e.get("msg", {}).get("type") == "shutdown_complete"
                        for e in self._events
                    )
                if done:
                    break
                time.sleep(0.05)
        except Exception:
            pass
        finally:
            try:
                if self._proc.stdin is not None:
                    self._proc.stdin.close()
            except Exception:
                pass
            try:
                self._proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._proc.kill()
                self._proc.wait()
        return self._proc.returncode

    def __enter__(self) -> "ServeSession":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()


# ── CLI 助手 ─────────────────────────────────────────────────────────────


def run_cli(
    sandbox: Sandbox,
    *args: str,
    env: dict[str, str] | None = None,
    timeout: float = 60.0,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    """跑 `reflect <args>`,返回 CompletedProcess(stdout/stderr 为文本)。"""
    return subprocess.run(
        [str(REFLECT_BIN), *args],
        cwd=str(sandbox.cwd),
        env=sandbox.env(env),
        capture_output=True,
        text=True,
        timeout=timeout,
        input=input_text,
    )


# ── 会话持久化 ───────────────────────────────────────────────────────────


def rollout_files(home: Path) -> list[Path]:
    """会话 JSONL 文件列表(`~/.reflect/sessions/YYYY/MM/DD/<thread>.jsonl`)。"""
    base = home / ".reflect" / "sessions"
    return sorted(base.glob("*/*/*/*.jsonl"))


def rollout_records(home: Path) -> list[dict[str, Any]]:
    """读全部会话记录的 `type` 字段列表(按文件顺序)。"""
    out: list[dict[str, Any]] = []
    for f in rollout_files(home):
        for line in f.read_text().splitlines():
            if line.strip():
                out.append(json.loads(line))
    return out


def rollout_thread_id(home: Path) -> str:
    """从**文件名**取 thread id。

    关键:`session_configured.session_id` 与会话文件名里的 thread id
    **不是同一个** —— exec/serve 路径没有注入 `with_session_id`,core 用
    `cfg.session_id.unwrap_or_default()`,而 `ThreadId::default()` 是新的
    随机 uuid。`--resume` 的参数必须用文件名的 id。
    """
    files = rollout_files(home)
    if not files:
        raise AssertionError(f"未找到会话文件:{home}/.reflect/sessions")
    return files[-1].stem


# ── 通用助手 ─────────────────────────────────────────────────────────────


def msg_types(events: list[dict[str, Any]]) -> list[str]:
    """事件流的消息类型序列。"""
    return [e.get("msg", {}).get("type", "") for e in events]


def first_event(
    events: list[dict[str, Any]], msg_type: str
) -> dict[str, Any] | None:
    """取第一个指定类型事件。"""
    for e in events:
        if e.get("msg", {}).get("type") == msg_type:
            return e
    return None


def wait_for(
    predicate: Callable[[], bool],
    timeout: float = 15.0,
    message: str = "条件未在时限内成立",
) -> None:
    """轮询等待条件成立;超时抛断言(回归时快速失败,不拖死套件)。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise AssertionError(f"{timeout}s 内{message}")


def require_binary() -> None:
    """被测二进制不存在时跳过整个模块(而不是失败)。"""
    import pytest

    if not REFLECT_BIN.is_file():
        pytest.skip(f"{REFLECT_BIN} 不存在;先执行 cargo build --release -p reflect-cli")


def mock_mcp_available() -> bool:
    return MOCK_MCP.is_file()
