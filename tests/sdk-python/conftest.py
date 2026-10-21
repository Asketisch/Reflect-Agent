"""Python SDK 绑定层的共享 fixture。

**这一层测什么**:Python SDK 客户端**自己**的行为 —— 迭代器何时收尾、
handler 怎么被调、回执怎么写、close 怎么收尾。
**不测什么**:服务端发什么(那是能力覆盖层 `tests/functional/` 的职责)。

两者分开的原因:SDK 自身的 bug 会把 Rust 的 bug 遮住(实例:Python SDK 的
`close()` 从不发出 shutdown op —— 见 tests/COVERAGE.md「已知缺陷」D1)。
"""

from __future__ import annotations

import json
import os
import shutil
import tempfile
import threading
from pathlib import Path
from typing import Any, Iterator

import pytest

# 仓库根:tests/sdk-python/conftest.py → 上溯两级。
ROOT = Path(__file__).resolve().parents[2]
# 被测二进制:默认 release 产物(与 tests/lib.sh 的约定一致)。
REFLECT_BIN = Path(
    os.environ.get("REFLECT_BIN", str(ROOT / "target/release/reflect"))
)
# 迭代器墙钟预算:终态事件缺失(回归)时断言失败,绝不挂死整个套件。
DRAIN_TIMEOUT = 30.0


@pytest.fixture()
def env_home() -> Iterator[Path]:
    """隔离 HOME:预写关闭全部内置 hook 的最小 config.toml。

    exec/serve 默认启用 VerificationHook(Stop 时跑 `cargo test`)等重 hook,
    单轮会拖到分钟级;SDK 场景只需协议语义,统一关掉。config.toml 必须存在,
    否则 ConfigWatcher 启动即报错退出。
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
    for var in (
        "REFLECT_MODEL",
        "REFLECT_MOCK_SCRIPT",
        "REFLECT_APPROVALS",
        "REFLECT_REMOTE_TOOL_TIMEOUT_SECS",
    ):
        os.environ.pop(var, None)
    if old_home is not None:
        os.environ["HOME"] = old_home
    if not os.environ.get("REFLECT_KEEP"):
        shutil.rmtree(home, ignore_errors=True)
