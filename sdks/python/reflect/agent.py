"""ReflectAgent —— Python SDK 主类。

最小用法::

    from reflect import ReflectAgent, ToolOutput, ContentBlock

    agent = ReflectAgent.spawn()
    await agent.register_tool(
        name="get_weather",
        description="查询指定城市的天气",
        parameters={"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
        handler=lambda args: ToolOutput(
            content=[ContentBlock(type="text", text=f"{args['city']} 晴")],
            is_error=False,
        ),
    )
    for ev in agent.submit("北京天气如何?"):
        msg = ev["msg"]
        if msg["type"] == "agent_message_delta":
            print(msg["delta"], end="", flush=True)
        if msg["type"] == "turn_complete":
            break
    agent.close()

底层:spawn `reflect serve` 子进程,JSONL stdio 协议;reader 线程把
stdout 行缓冲到内存队列,按 submission id 路由到迭代器。
"""

from __future__ import annotations

import json
import os
import queue
import shutil
import subprocess
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Optional

from .protocol import (
    EVENT_ID_NONE,
    ContentBlock,
    Event,
    EventMsg,
    Op,
    RemoteToolSpec,
    Submission,
    ToolOutput,
)


REFLECT_BIN_ENV = "REFLECT_BIN"
DEFAULT_BIN = "reflect"
DEFAULT_HANDSHAKE_TIMEOUT = 30.0


ToolHandler = Callable[[dict[str, Any]], ToolOutput]


@dataclass
class ReflectAgentOptions:
    """ReflectAgent.spawn 的可选配置。"""

    bin: Optional[str] = None
    cwd: Optional[Path] = None
    env: Optional[dict[str, str]] = None
    handshake_timeout_secs: float = DEFAULT_HANDSHAKE_TIMEOUT


@dataclass
class _RegisteredTool:
    spec: RemoteToolSpec
    handler: ToolHandler


def _text_output(text: str, *, is_error: bool = False) -> ToolOutput:
    return ToolOutput(
        content=[ContentBlock(type="text", text=text)],
        is_error=is_error,
        metadata={},
        elapsed_ms=0,
    )


def _is_terminal(msg: dict[str, Any]) -> bool:
    t = msg.get("type")
    return t in ("turn_complete", "turn_aborted", "shutdown_complete")


class ReflectAgent:
    """与 `reflect serve` 子进程保持常驻会话的 Python 主类。

    线程模型:
    - 主线程负责用户调用 / 写 Submission / 调度按 id 路由;
    - **reader 线程**专门消费 stdout,逐行解析 Event,把全局事件
      (`id=""`) 与按 submission id 路由的事件分别入队。

    这样 `submit()` / `prompt()` 返回的迭代器天然支持并发多轮 —— 每条
    submission 互不干扰。
    """

    def __init__(
        self,
        proc: subprocess.Popen[bytes],
        options: ReflectAgentOptions,
    ) -> None:
        self._proc = proc
        self._closed = False
        self._tools: dict[str, _RegisteredTool] = {}
        self._global_queue: queue.Queue[dict[str, Any]] = queue.Queue()
        # sub.id → 订阅该 submission 事件的回调
        self._sub_listeners: dict[str, Callable[[dict[str, Any]], None]] = {}
        self._sub_lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._handshake_event = threading.Event()
        self._reader_thread = threading.Thread(
            target=self._reader_loop, name="reflect-reader", daemon=True
        )
        self._reader_thread.start()
        self._wait_handshake(options.handshake_timeout_secs)

    # ── 公开 API ──────────────────────────────────────────────────────

    @classmethod
    def spawn(cls, options: Optional[ReflectAgentOptions] = None) -> "ReflectAgent":
        """启动 `reflect serve` 子进程,等到 `session_configured` 即返回。"""
        options = options or ReflectAgentOptions()
        bin_path = (
            options.bin
            or os.environ.get(REFLECT_BIN_ENV)
            or shutil.which(DEFAULT_BIN)
        )
        if not bin_path:
            raise RuntimeError(
                "reflect binary not found. Install it on PATH or set REFLECT_BIN."
            )
        env = dict(os.environ)
        if options.env:
            env.update(options.env)
        # 不传 bufsize(默认 buffered text mode),reader 线程可走
        # `readline()` 文本逐行读取。`bufsize=0` 会让 stdout 退化为
        # raw `FileIO` —— 没有 `readline`,文本 JSONL 处理就崩。
        proc = subprocess.Popen(
            [bin_path, "serve"],
            cwd=options.cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        # stderr 透传到当前 stderr(让 tracing 日志可见)。
        threading.Thread(
            target=_forward_stream,
            args=(proc.stderr, 2),
            daemon=True,
        ).start()
        return cls(proc, options)

    @classmethod
    def from_popen(
        cls,
        proc: subprocess.Popen[bytes],
        handshake_timeout_secs: float = DEFAULT_HANDSHAKE_TIMEOUT,
    ) -> "ReflectAgent":
        """测试 / 注入用:用现成的 subprocess.Popen 代替 spawn。

        `proc.stdin` / `proc.stdout` 必须为非 None。该路径不转发 stderr。
        """
        options = ReflectAgentOptions(handshake_timeout_secs=handshake_timeout_secs)
        return cls(proc, options)

    def register_tool(
        self,
        name: str,
        description: str,
        parameters: dict[str, Any],
        handler: ToolHandler,
    ) -> None:
        """注册客户端自定义工具(LLM 调用时由本地 handler 返回结果)。

        立即 emit `Op::RegisterTools` 到 serve;同步等待回执 ack 没有
        意义(Rust 端静默接受,错误以 `EventMsg::Error` 异步回报)。
        """
        spec = RemoteToolSpec(
            name=name,
            description=description,
            parameters=parameters,
        )
        self._tools[name] = _RegisteredTool(spec, handler)
        self._write_submission(_build_submission({"type": "register_tools", "tools": [spec]}))

    def submit(self, text: str) -> Iterator[dict[str, Any]]:
        """提交一条用户 prompt;返回该 submission 的事件迭代器(字典)。

        迭代器结束条件:`turn_complete` / `turn_aborted` / `shutdown_complete`,
        或与子进程的连接断开。
        """
        op: Op = {
            "type": "user_input",
            "items": [{"type": "text", "text": text}],
        }
        return self._submit_op(op)

    def submit_op(self, op: Op, *, submission_id: Optional[str] = None) -> Iterator[dict[str, Any]]:
        return self._submit_op(op, submission_id=submission_id)

    def prompt(self, text: str) -> str:
        """高层便捷:`prompt(text)` → 最终 assistant 文本。

        聚合 `agent_message_delta` / `agent_message`,遇到 `turn_complete`
        结束;`turn_aborted` / `error` 抛错。
        """
        out: list[str] = []
        for ev in self.submit(text):
            msg = ev["msg"]
            t = msg.get("type")
            if t == "agent_message_delta":
                out.append(msg["delta"])
            elif t == "agent_message":
                out.append(msg["text"])
            elif t == "turn_complete":
                break
            elif t == "turn_aborted":
                raise RuntimeError(f"reflect turn aborted: {msg.get('reason')}")
            elif t == "error":
                raise RuntimeError(f"reflect error: {msg.get('code')}: {msg.get('message')}")
        return "".join(out)

    def interrupt(self, child_id: Optional[str] = None) -> None:
        self._write_submission(
            _build_submission({"type": "interrupt", "child_id": child_id})
        )

    def approve(
        self,
        approval_id: str,
        decision: dict[str, Any],
    ) -> None:
        """响应 `EventMsg::ApprovalRequest` / `HookApprovalRequest`。"""
        self._write_submission(
            _build_submission(
                {"type": "tool_approval", "id": approval_id, "decision": decision}
            )
        )

    def close(self) -> None:
        """发 Shutdown → 等 ShutdownComplete → 关子进程。幂等。"""
        if self._closed:
            return
        self._closed = True
        try:
            self._write_submission(_build_submission({"type": "shutdown"}))
        except Exception:
            pass
        # 关 stdin 让 mock 端感知 EOF;真 serve 会在 Shutdown 后 exit。
        if self._proc.stdin:
            try:
                self._proc.stdin.close()
            except Exception:
                pass
        # 等待子进程优雅退出(短上限,超时强杀)。
        try:
            self._proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()

    # ── 内部 ──────────────────────────────────────────────────────────

    def _wait_handshake(self, timeout_secs: float) -> None:
        if not self._handshake_event.wait(timeout=timeout_secs):
            self.close()
            raise RuntimeError(
                f"reflect serve handshake timed out after {timeout_secs}s"
            )

    def _reader_loop(self) -> None:
        """消费 stdout,逐行解析 Event;分发到全局 / 按 submission id 的队列。

        同时兼容 bytes 与 str:subprocess 默认(非 text 模式)stdout 是
        `BufferedReader`,`readline()` 返回 bytes、EOF 返回 `b""`;若调用方
        传入了 text 流则返回 str、EOF 返回 `""`。两种空值都为 falsy,
        统一用 `if not raw: break` 判断,避免 `iter(callable, sentinel)`
        在哨兵类型不匹配时的隐式行为(bytes 流下 `""` 哨兵永不命中,
        会无限自旋)。
        """
        assert self._proc.stdout is not None
        while True:
            try:
                raw = self._proc.stdout.readline()
            except Exception:
                break
            if not raw:  # EOF:b"" / ""
                break
            if isinstance(raw, bytes):
                line = raw.decode("utf-8", errors="replace")
            else:
                line = raw
            line = line.rstrip("\r\n")
            if not line:
                continue
            try:
                obj = json.loads(line)
            except (json.JSONDecodeError, TypeError):
                continue
            self._dispatch(obj)
        self._on_eof()

    def _dispatch(self, obj: dict[str, Any]) -> None:
        """单条 Event 路由:握手、远程工具请求、全局队列或按 id 回调。"""
        msg = obj.get("msg") or {}
        if msg.get("type") == "session_configured" and not self._handshake_event.is_set():
            self._handshake_event.set()
        # 远程工具请求在 wire 上是全局事件(id="",见 PROTOCOL.md §5):
        # 无论 id 是什么都先于路由拦截,交给注册的本地 handler 执行。
        # handler 在独立线程跑 —— reader 线程必须继续消费 stdout(并行
        # 多次工具调用 / handler 内部再调 prompt() 都不能卡死读取)。
        if msg.get("type") == "tool_execution_request":
            threading.Thread(
                target=self._handle_tool_request_safely,
                args=(msg.get("call_id", ""), msg.get("tool", ""), msg.get("args") or {}),
                daemon=True,
            ).start()
            return
        ev_id = obj.get("id", "")
        if ev_id == EVENT_ID_NONE:
            self._global_queue.put(obj)
            return
        with self._sub_lock:
            cb = self._sub_listeners.get(ev_id)
        if cb is not None:
            cb(obj)

    def _on_eof(self) -> None:
        """stdout EOF:唤醒所有仍挂起的 submission 监听者,结束其迭代器。"""
        with self._sub_lock:
            stale = list(self._sub_listeners.items())
            self._sub_listeners.clear()
        for _sub_id, cb in stale:
            cb(None)
        self._global_queue.put(None)

    def _submit_op(
        self,
        op: Op,
        *,
        submission_id: Optional[str] = None,
    ) -> Iterator[dict[str, Any]]:
        sub_id = submission_id or str(uuid.uuid4())
        # 必须在写 stdin 之前注册 listener,否则首批事件可能在注册前到达。
        # 用 queue 隔离 listener 提前关闭 vs 收尾。
        q: queue.Queue[Optional[dict[str, Any]]] = queue.Queue()
        def _push(ev: Optional[dict[str, Any]]) -> None:
            q.put(ev)
        with self._sub_lock:
            self._sub_listeners[sub_id] = _push
        self._write_submission(_build_submission(op, sub_id))
        return _SubmissionIterator(q, sub_id, self)

    def _write_submission(self, sub: dict[str, Any]) -> None:
        if self._closed:
            raise RuntimeError("agent is closed")
        assert self._proc.stdin is not None
        line = json.dumps(sub) + "\n"
        # 写锁:远程工具回执来自多个 worker 线程,与主线程的 submit /
        # approve 并发写 stdin 时避免行交错。
        with self._write_lock:
            self._proc.stdin.write(line.encode("utf-8"))
            self._proc.stdin.flush()

    def _handle_tool_request_safely(
        self,
        call_id: str,
        tool: str,
        args: dict[str, Any],
    ) -> None:
        """worker 线程入口:执行 handler 并回执;异常兜底不炸线程。"""
        try:
            self._handle_tool_request(call_id, tool, args)
        except Exception:
            pass

    def _handle_tool_request(
        self,
        call_id: str,
        tool: str,
        args: dict[str, Any],
    ) -> None:
        entry = self._tools.get(tool)
        if entry is None:
            self._write_submission(
                _build_submission(
                    {
                        "type": "tool_execution_response",
                        "call_id": call_id,
                        "output": _text_output(
                            f"tool '{tool}' not registered in this SDK",
                            is_error=True,
                        ),
                    }
                )
            )
            return
        try:
            output = entry.handler(args)
        except Exception as exc:  # noqa: BLE001
            self._write_submission(
                _build_submission(
                    {
                        "type": "tool_execution_response",
                        "call_id": call_id,
                        "output": _text_output(str(exc), is_error=True),
                    }
                )
            )
            return
        self._write_submission(
            _build_submission(
                {
                    "type": "tool_execution_response",
                    "call_id": call_id,
                    "output": output,
                }
            )
        )


class _SubmissionIterator(Iterator[dict[str, Any]]):
    """按 submission id 路由的事件迭代器。"""

    def __init__(
        self,
        q: queue.Queue[Optional[dict[str, Any]]],
        sub_id: str,
        agent: ReflectAgent,
    ) -> None:
        self._q = q
        self._sub_id = sub_id
        self._agent = agent
        # 终态事件(turn_complete 等)已 yield 后置 True:再次 __next__
        # 直接 StopIteration,否则会永久阻塞在 q.get()(listener 已摘除,
        # 不会再有事件入队)。
        self._done = False

    def __iter__(self) -> "_SubmissionIterator":
        return self

    def __next__(self) -> dict[str, Any]:
        if self._done:
            raise StopIteration
        ev = self._q.get()
        if ev is None:
            # EOF / 通道关闭:抛出 StopIteration 即可。
            raise StopIteration
        msg = ev.get("msg") or {}
        if _is_terminal(msg):
            self._done = True
            # 收尾:从 agent 注册表中移除 listener(若还在)。
            with self._agent._sub_lock:
                self._agent._sub_listeners.pop(self._sub_id, None)
        return ev


# ── helpers ───────────────────────────────────────────────────────────────


def _build_submission(op: dict[str, Any], sub_id: Optional[str] = None) -> dict[str, Any]:
    return {"id": sub_id or str(uuid.uuid4()), "op": op}


def _forward_stream(src: Any, dst_fd: int) -> None:
    """把 src 流逐块写到 dst_fd(1=stdout / 2=stderr)。

    简单方案:写到 sys.stdout / sys.stderr 的 .buffer。失败时静默退出,
    子进程关闭路径不应该让主进程崩。
    """
    import sys
    target = sys.stdout if dst_fd == 1 else sys.stderr if dst_fd == 2 else None
    if target is None:
        return
    try:
        for chunk in iter(src.read1, b""):
            target.buffer.write(chunk)
            target.flush()
    except Exception:
        return
    finally:
        try:
            src.close()
        except Exception:
            pass