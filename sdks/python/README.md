# zhuye (Python SDK)

让 Python 项目直接驱动 [ZhuYe Agent](https://github.com/Asketisch/ZhuYe) 的官方 SDK。
底层 spawn `zhuye serve` 子进程、走 JSONL stdio 协议:**安装即用**(平台 wheel
内嵌对应平台的 `zhuye` 二进制)、**零运行时第三方依赖**(仅 stdlib)、
**协议 v0 冻结**(Rust 核心新增变体自动降级,不断旧客户端)。

- Python ≥ 3.10,零依赖,含 `py.typed` 类型标注
- 常驻会话:一个进程 = 一个 AgentThread,多轮共享上下文
- 宿主语言函数直接注册为 LLM 可调用的工具(远程工具协议)
- 支持 streaming、审批、打断、全局事件、权限模式、计划 / 目标模式

## 安装

```bash
pip install zhuye
```

安装即用:PyPI 提供平台 wheel(macOS arm64 / x86_64,Linux x86_64 /
aarch64 glibc,Windows x86_64),内嵌对应平台的 `zhuye` 二进制,SDK
优先解析它 —— 无需自装二进制、无需配环境变量。

**二进制解析优先级**(双官方 SDK 一致):

```
options.bin  →  ZHUYE_BIN  →  wheel 内嵌二进制  →  PATH 上的 zhuye
```

当前平台不在支持矩阵(如 musl/Alpine)时回退 PATH:从
[GitHub Releases](https://github.com/Asketisch/ZhuYe/releases) 下载安装包,
或从源码 `make build` 产出 `target/release/zhuye` 后以 `ZHUYE_BIN` 指向。

## 快速开始

```python
from zhuye import ZhuyeAgent

agent = ZhuyeAgent.spawn()          # 等 session_configured 握手完成才返回
print(agent.prompt("用一句话介绍你自己"))
agent.close()                        # 幂等:Shutdown → 等退出 → 强杀兜底
```

默认走环境变量里的模型配置;离线体验用 mock 模型:`ZHUYE_MODEL=mock`。

## 核心概念

```
Python 进程                                zhuye serve 子进程
┌─────────────────────┐    JSONL stdio    ┌──────────────────────┐
│ ZhuyeAgent          │ ── Submission ──▶ │ AgentThread          │
│  reader 线程        │ ◀──── Event ───── │  (StateGraph 回合)   │
│  ├ id="" 全局队列   │                   │                      │
│  └ 按 submission id ─                   │                      │
│    路由到迭代器      │                   │                      │
└─────────────────────┘                   └──────────────────────┘
```

- **常驻会话**:`spawn()` 拉起的 serve 进程在 `close()` 前一直存活,
  多轮 `submit()` 共享完整对话上下文。
- **事件双通道**:事件分两类 ——
  - *submission 事件*:挂在提交 id 上,由 `submit()` 返回的迭代器独占消费
    (流式 delta、turn_complete、tool_call_end 等);
  - *全局事件*(`id=""`):不属于任何提交(token_count、permission_mode_changed、
    握手 session_configured 等),由 `next_global_event()` 读取。
  两通道互不混入。远程工具请求(`tool_execution_request`)虽在 wire 上是
  全局事件,但 SDK 内部拦截并调度本地 handler,不会出现在任何一个队列里。
- **迭代器收尾**:`turn_complete` / `turn_aborted` / `shutdown_complete` /
  `submission_closed` 为终态事件,迭代器在其后自然结束;连接断开(EOF)
  也会唤醒并结束所有迭代器,不会永久挂起。

## 使用指南

### 流式输出(submit + delta)

```python
from zhuye import ZhuyeAgent

agent = ZhuyeAgent.spawn()
for ev in agent.submit("讲个笑话"):
    msg = ev["msg"]
    if msg["type"] == "agent_message_delta":
        print(msg["delta"], end="", flush=True)
    elif msg["type"] == "turn_complete":
        print(f"\n[turn 结束] 用量 {msg['usage']}")
agent.close()
```

`submit(text)` 内部把文本包成 `user_input` Op,返回**该次提交专属**的
事件迭代器;迭代结束即本次 turn 结束。

### 一步拿全文(prompt)

```python
answer = agent.prompt("北京天气如何?")
```

聚合所有 delta;`turn_complete` 正常返回,`turn_aborted` / `error` 事件
抛 `RuntimeError`(部分文本不会被当作完整答案静默返回)。

### 自定义工具(宿主函数 → LLM 工具)

```python
from zhuye import ZhuyeAgent, ContentBlock, ToolOutput

def get_weather(args: dict) -> ToolOutput:
    return ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴 26℃")],
        is_error=False,
        metadata={},
        elapsed_ms=0,
    )

agent = ZhuyeAgent.spawn()
agent.register_tool(
    name="get_weather",
    description="查询指定城市的天气",
    parameters={                       # JSON Schema
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
    },
    handler=get_weather,
)
print(agent.prompt("上海天气怎么样?"))
```

行为约定(协议 §5):

- 模型调用工具时,serve 下发 `tool_execution_request`,SDK 在**独立
  线程**执行 handler 并回执;reader 线程不阻塞,handler 里可以再调
  `prompt()` / `submit()`。
- handler 抛异常 → SDK 自动转成 `is_error=True` 的文本回执,LLM 看到
  错误信息,turn 不挂死。
- 未注册的工具被调用 → 回执错误文本(同样不挂死)。
- 等待回执上限由环境变量 `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` 控制(默认 120s),
  超时以错误收尾。

### 多轮与并发

```python
# 多轮共享上下文
agent.prompt("我叫小明")
agent.prompt("我叫什么?")            # 模型能答出"小明"

# 并发提交:每次 submit 独立 id 与迭代器,多线程安全
import threading
t1 = threading.Thread(target=lambda: [print(e) for e in agent.submit("问题一")])
t2 = threading.Thread(target=lambda: [print(e) for e in agent.submit("问题二")])
t1.start(); t2.start(); t1.join(); t2.join()
```

### 打断与审批

```python
# 打断当前 turn(随后该 turn 以 turn_aborted 收尾)
agent.interrupt()

# 审批:收到全局事件 approval_request 后回决策
import json
event = agent.next_global_event(timeout_secs=30)
if event and event["msg"]["type"] == "approval_request":
    req = event["msg"]
    agent.approve(req["request_id"], "approve")            # 或 "approve_for_session"
    # 拒绝:agent.approve(req["request_id"], {"deny": {"reason": "太危险"}})
```

`decision` 透传 wire 形态,不做形状转换:`"approve"` /
`"approve_for_session"`,或 `{"deny": {"reason": "..."}}`(旧版
`{"type":"approve"}` 无法被服务端反序列化,勿用)。Hook 审批使用同一
decision 形态(serve 按事件种类自行分发)。

### 全局事件

```python
event = agent.next_global_event(timeout_secs=5)   # 阻塞读一条;超时抛 queue.Empty
```

- 握手 `session_configured` 本身也在全局队列里,通常第一条就是它;
- stdout EOF / `close()` 后返回 `None`,重复读取安全;
- 与 submission 迭代器完全独立,未知变体原样返回。

### 会话控制 Op(submit_op)

`submit_op(op)` 发任意协议 Op 并返回事件迭代器(非 turn Op 以
`submission_closed` 收尾)。Op 全集(19 种,`zhuye.Op`):

| 类别 | Op | 关键字段 |
|---|---|---|
| 对话 | `user_input` | `items`(text/image)、可选 `thread_settings{effort, permission_mode, model}` |
| 对话 | `compact` / `rewind` | rewind 可选 `to_turn_id` |
| 控制 | `interrupt` / `shutdown` | interrupt 可选 `child_id` |
| 审批 | `tool_approval` / `hook_approval` | `id` + `decision` |
| 计划 | `enter_plan_mode` / `exit_plan_mode` / `plan_approval` | `plan_approval.choice ∈ auto_mode/manual_approve/revise` |
| 交互 | `ask_user_question_response` / `ask_user_input_response` | 回应全局事件 ask_user_question / ask_user_input |
| 状态 | `set_effort` / `set_permission_mode` / `cycle_permission_mode` | mode ∈ auto/prompt/plan/deny/bubble |
| 目标 | `enter_goal_mode` / `exit_goal_mode` | goal + 可选 verify_command / token_budget |
| 工具 | `register_tools` / `tool_execution_response` | 通常用 `register_tool()` 封装,不必手写 |

示例:切计划模式、带 thread_settings 提交

```python
evs = list(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
evs = list(agent.submit_op({
    "type": "user_input",
    "items": [{"type": "text", "text": "调研一下"}],
    "thread_settings": {"effort": "high"},
}))
```

## API 参考

### `ZhuyeAgent`

| 方法 | 说明 |
|---|---|
| `ZhuyeAgent.spawn(options=None) -> ZhuyeAgent` | 拉起 `zhuye serve`,阻塞到握手完成;二进制找不到 / 握手超时抛 `RuntimeError` |
| `ZhuyeAgent.from_popen(proc, handshake_timeout_secs=30)` | 测试注入:用现成 `Popen`(stdin/stdout 必须为管道)代替 spawn;该路径不转发 stderr |
| `register_tool(name, description, parameters, handler)` | 注册远程工具,任意时刻可调;静默接受,错误异步以 error 事件回报 |
| `submit(text) -> Iterator[dict]` | 提交用户文本,返回该提交的事件迭代器 |
| `submit_op(op, *, submission_id=None) -> Iterator[dict]` | 提交任意 Op;可显式指定 submission id |
| `prompt(text) -> str` | 聚合 delta 返回全文;`turn_aborted` / `error` 抛 `RuntimeError` |
| `interrupt(child_id=None)` | 打断在飞 turn |
| `approve(approval_id, decision)` | 回审批决策(wire 形态透传) |
| `next_global_event(timeout_secs=None) -> dict \| None` | 读全局事件;超时抛 `queue.Empty`,EOF 后返回 `None` |
| `close()` | 幂等关闭:Shutdown → 等 2s → 强杀;之后所有写入静默丢弃,迭代器收尾 |

### `ZhuyeAgentOptions`

| 字段 | 默认 | 说明 |
|---|---|---|
| `bin` | `None` | 二进制路径;缺省走解析优先级(见安装) |
| `cwd` | `None` | serve 进程工作目录 |
| `env` | `None` | 追加环境变量(在父环境上合并,如 `ZHUYE_MODEL=mock`) |
| `handshake_timeout_secs` | `30.0` | `session_configured` 握手超时 |

### 数据类型(`from zhuye import ...`)

- `ToolOutput`:工具回执 `{content: [ContentBlock], is_error, metadata, elapsed_ms}`(字段均可选,惯用全给)。
- `ContentBlock`:`{type: "text", text: str}`。
- `RemoteToolSpec`:`{name, description, parameters(JSON Schema)}`。
- `Event`:`{id: str, msg: dict}`;`id=""` 为全局事件。
- `Op` / `EventMsg` / `UnknownEventMsg`:协议 TypedDict(见下)。
- `Submission` / `EVENT_ID_NONE`。

## 事件参考

迭代器与全局队列中的 `msg["type"]`(完整类型见 `zhuye/protocol.py`,
与 Rust `EventMsg` 一一对应):

| 变体 | 通道 | 说明 |
|---|---|---|
| `session_configured` | 全局 | 握手(session_id / model / provider / 策略) |
| `turn_started` / `turn_complete` | submission | 回合起止;complete 带 `status` 与 `usage` |
| `turn_aborted` | submission | 回合被打断,`reason ∈ user_interrupt/shutdown/error` |
| `agent_message` / `agent_message_delta` | submission | 最终文本 / 流式增量 |
| `thinking_delta` | submission | 思考流增量 |
| `token_count` | 全局 | 用量统计 |
| `tool_call_begin` / `tool_call_args_delta` / `tool_call_output_delta` / `tool_call_end` | submission | 内置工具调用四件套;end 带 `is_error` 与最终 output |
| `tool_execution_request` | (SDK 内部拦截) | 远程工具请求 → 本地 handler |
| `approval_request` / `permission_bubble` | 全局 | 需要审批 → `approve()` 回应 |
| `ask_user_question` / `ask_user_input` | 全局 | 需要用户作答 → `submit_op` 回应对应 response Op |
| `permission_mode_changed` | 全局 | 权限模式切换结果 |
| `context_compacted` | submission | 压缩完成 |
| `plan_request` / `plan_ready` / `plan_approved` / `plan_rejected` / `plan_draft_updated` / `plan_step` | submission / 全局 | 计划模式事件 |
| `error` / `stream_error` | submission | 错误(`error` 会被 `prompt()` 转为异常) |
| `shutdown_complete` | submission | 关闭完成 |
| `submission_closed` | submission | 该提交通道排空(非 turn Op 的收尾标记) |
| `turn_rewound` / `config_reloaded` / `collab_*` / `mcp_server_*` / `mcp_tool_invoked` / `lsp_server_*` / `plugin_loaded` / `quota_exhausted` / `routing` | — | 其余变体见 protocol.py |
| `UnknownEventMsg` 形态 | 任意 | 未识别变体降级(`type` 保留原值),保证前向兼容 |

## 错误处理速查

| 场景 | 表现 |
|---|---|
| 找不到二进制 | `spawn()` 抛 `RuntimeError`(按优先级解析失败) |
| 握手超时 / 进程秒退 | `spawn()` 抛 `RuntimeError`(超时前会先 `close()`) |
| 模型返回 error 事件 | `prompt()` 抛 `RuntimeError: zhuye error: <code>: <message>`;`submit()` 流里自行处理 |
| turn 被打断 | `prompt()` 抛 `RuntimeError: zhuye turn aborted: ...`;`submit()` 流里见 `turn_aborted` |
| 全局事件读超时 | `next_global_event` 抛 `queue.Empty` |
| handler 抛异常 | 自动转 `is_error=True` 回执,不影响 turn;不影响 SDK |
| `close()` 后写入 | 静默丢弃(回执无处可去,不炸宿主) |

## 环境变量

| 变量 | 作用 |
|------|------|
| `ZHUYE_BIN` | `zhuye` 二进制路径(最高优先级覆盖;缺省用 wheel 内嵌二进制)。 |
| `ZHUYE_MODEL` | 模型 spec(`mock/mock-1` 用于离线测试)。 |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 测试

```bash
python3 -m pip install pytest
python3 -m pytest -ra
```

mock serve 进程跑在 Node 上(Node ≥ 18),不依赖真 `zhuye` 二进制 —— 测试
完全离线,也不需要网络/API key。仓库内开发直接以源码树运行
(`python -m pytest` 会把本目录加入 `sys.path`),无需构建安装;打包后端为
maturin(平台 wheel 由 CI 产出并内嵌二进制),本地构建需 Rust 工具链。

端到端测试(对真 `zhuye serve` 子进程的集成)在仓库 CI 里跑:配置好
`ZHUYE_BIN` 后执行 `./tests/run_all.sh sdk-python`。

## 协议与版本兼容

- SDK 与 `zhuye serve` 之间的 wire 协议见 [`../PROTOCOL.md`](../PROTOCOL.md)
  (线上:[GitHub](https://github.com/Asketisch/ZhuYe/blob/main/sdks/PROTOCOL.md))。
- 协议 v0 已冻结:Rust 核心新增 Op / EventMsg 变体属 additive 扩展,SDK
  遇到未识别变体降级为 `UnknownEventMsg` 原样透传,不会崩。
- SDK 版本与 Rust workspace 版本统一管理(release 流水线保证四处一致)。

## 许可证

MIT。仓库:<https://github.com/Asketisch/ZhuYe>。
