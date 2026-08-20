"""reflect-agent Python SDK —— 让脚本项目能直接驱动 Reflect Agent。

底层通过 spawn `reflect serve` 子进程、走 JSONL stdio 协议,**零原生
依赖、零 Python 运行时第三方包**(仅 stdlib)。需要本机 `reflect` 二
进制在 PATH(或通过 `REFLECT_BIN` 指向)。

最小示例::

    from reflect import ReflectAgent

    agent = ReflectAgent.spawn()
    text = agent.prompt("hello")
    print(text)
    agent.close()

更复杂的场景(自定义工具、多轮、错误处理)见 README.md。
"""

from __future__ import annotations

from .agent import (
    ContentBlock,
    Event,
    ReflectAgent,
    ReflectAgentOptions,
    ToolHandler,
    ToolOutput,
)
from .protocol import (
    EVENT_ID_NONE,
    EventMsg,
    Op,
    RemoteToolSpec,
    Submission,
    UnknownEventMsg,
)

__all__ = [
    "ReflectAgent",
    "ReflectAgentOptions",
    "ToolHandler",
    "ToolOutput",
    "ContentBlock",
    "Op",
    "Submission",
    "Event",
    "EventMsg",
    "UnknownEventMsg",
    "RemoteToolSpec",
    "EVENT_ID_NONE",
]

__version__ = "0.1.0"