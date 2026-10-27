"""能力覆盖层的 pytest fixture(唯一职责:提供隔离环境与被测对象)。

共享设施在 `wire.py`(唯一命名模块)—— 不用 `from conftest import ...` 的写法:
两个测试层各有一个 conftest.py,同名导入在一次性收集整个 tests/ 时会按
sys.path 顺序解析到错误的那一份。
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable, Iterator

import pytest

from wire import DRAIN_TIMEOUT, ZHUYE_BIN, Sandbox, ServeSession


@pytest.fixture()
def sandbox(tmp_path: Path) -> Iterator[Sandbox]:
    """每个用例一份隔离 HOME + cwd。

    HOME 里预写 `~/.zhuye/config.toml`(`[hooks] enabled = []`):serve/CLI
    默认启用 VerificationHook(Stop 时跑 `cargo test`)等重 hook,单轮会拖到
    分钟级;能力用例只验行为语义,不验 hook 计时。config.toml 必须存在 ——
    否则 ConfigWatcher 启动即报错退出。
    """
    home = tmp_path / "home"
    cwd = tmp_path / "cwd"
    (home / ".zhuye").mkdir(parents=True)
    cwd.mkdir(parents=True)
    (home / ".zhuye" / "config.toml").write_text(
        "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n"
    )
    yield Sandbox(home=home, cwd=cwd)


@pytest.fixture()
def mock_script(tmp_path: Path) -> Callable[[list[str]], Path]:
    """写 mock provider 脚本:JSONL,每行一次模型回复。

    行格式(见 crates/abilities/zhuye-llm/src/providers/mock.rs):
    `{"type":"text","text":"..."}` / `{"type":"tool_call","name":"t","args":{}}`
    """

    def _write(lines: list[str]) -> Path:
        path = tmp_path / "mock-script.jsonl"
        path.write_text("\n".join(lines) + "\n")
        return path

    return _write



@pytest.fixture()
def serve_session(sandbox: Sandbox) -> Callable[..., ServeSession]:
    """起 `zhuye serve` 会话的工厂(自动收尾)。"""

    created: list[ServeSession] = []

    def _make(
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
        timeout: float = DRAIN_TIMEOUT,
    ) -> ServeSession:
        s = ServeSession(sandbox, args=args, env=env, timeout=timeout)
        created.append(s)
        return s

    yield _make
    for s in created:
        try:
            s.close(timeout=3.0)
        except Exception:
            pass


