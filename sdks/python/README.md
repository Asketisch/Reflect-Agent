# reflect-agent (Python SDK)

让 Python 项目直接驱动 Reflect Agent 的轻量 SDK。底层通过 spawn `reflect serve` 子进程、走 JSONL stdio 协议,**零运行时依赖**(仅 stdlib)。

## 安装

```bash
pip install reflect-agent
```

需要本机有 `reflect` 二进制在 PATH(或通过 `REFLECT_BIN` 指向)。从 [Releases](https://cnb.cool/...) 下载,或从本仓库 `make build` 产出 `target/release/reflect`。

## 用法

```python
from reflect import ReflectAgent, ToolOutput, ContentBlock

agent = ReflectAgent.spawn()

# 注册客户端自定义工具(LLM 调用时由本地函数返回结果)
def get_weather(args):
    return ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 今天晴 26℃")],
        is_error=False,
        metadata={},
        elapsed_ms=0,
    )

agent.register_tool(
    name="get_weather",
    description="查询城市天气",
    parameters={
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
    },
    handler=get_weather,
)

# 高层:聚合 delta,返回最终文本
print(agent.prompt("北京天气如何?"))

# 低层:遍历 turn 事件
for ev in agent.submit("第二轮"):
    msg = ev["msg"]
    if msg["type"] == "agent_message_delta":
        print(msg["delta"], end="", flush=True)
    if msg["type"] == "turn_complete":
        break

agent.close()
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `REFLECT_BIN` | `reflect` 二进制路径(默认搜索 PATH)。 |
| `REFLECT_MODEL` | 模型 spec(`mock/mock-1` 用于离线 SDK 集成测试)。 |
| `REFLECT_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 测试

```bash
pip install -e .[dev]
pytest -ra
```

mock serve 进程跑在 Node 上(Node ≥ 18),不依赖真 `reflect` 二进制 —— 测试完全离线,也不需要网络/API key。

端到端测试(对真 `reflect serve` 子进程的集成)在仓库 CI 里跑(配置好 `REFLECT_BIN` 后 `pytest tests/test_protocol.py`)。

## 协议

SDK 与 `reflect serve` 之间的 wire 协议见 `../PROTOCOL.md`(同仓库 `sdks/PROTOCOL.md`)。Rust 核心新增协议变体时,SDK 自动降为 `UnknownEventMsg`,不会中断下游。