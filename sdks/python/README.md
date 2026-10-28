# zhuye (Python SDK)

让 Python 项目直接驱动 Zhuye Agent 的轻量 SDK。底层通过 spawn `zhuye serve` 子进程、走 JSONL stdio 协议,**零运行时依赖**(仅 stdlib)。

## 安装

```bash
pip install zhuye
```

安装即用:PyPI 提供平台 wheel(macOS arm64 / x86_64,Linux x86_64 /
aarch64 glibc,Windows x86_64),内嵌对应平台的 `zhuye` 二进制,SDK
优先解析它 —— 无需自装二进制、无需配环境变量。

二进制解析优先级:显式 `options.bin` → `ZHUYE_BIN` → wheel 内嵌
二进制 → PATH 上的 `zhuye`。当前平台不在支持矩阵(如 musl/Alpine)
时回退 PATH:从 [GitHub Releases](https://github.com/Asketisch/ZhuYe/releases)
下载,或从本仓库 `make build` 产出 `target/release/zhuye` 后以
`ZHUYE_BIN` 指向。

## 用法

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()

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

## 全局事件

`next_global_event(timeout_secs=5)` 读取 `id=""` 的会话事件,与 `submit()` 的
按 submission 路由的事件迭代器独立。超时抛 `queue.Empty`,进程 stdout
关闭后返回 `None`;未知变体原样返回。

```python
handshake = agent.next_global_event(timeout_secs=5)
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `ZHUYE_BIN` | `zhuye` 二进制路径(最高优先级覆盖;缺省用 wheel 内嵌二进制)。 |
| `ZHUYE_MODEL` | 模型 spec(`mock/mock-1` 用于离线 SDK 集成测试)。 |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 测试

```bash
python3 -m pip install pytest
python3 -m pytest -ra
```

mock serve 进程跑在 Node 上(Node ≥ 18),不依赖真 `zhuye` 二进制 —— 测试完全离线,也不需要网络/API key。仓库内开发直接以源码树运行(`python -m pytest` 会把本目录加入 `sys.path`),无需构建安装;打包后端为 maturin(平台 wheel 由 CI 产出并内嵌二进制,见 `docs/release.md`),本地构建需 Rust 工具链。

端到端测试(对真 `zhuye serve` 子进程的集成)在仓库 CI 里跑;在仓库根配置好 `ZHUYE_BIN` 后执行 `./tests/run_all.sh sdk-python`。

## 协议

SDK 与 `zhuye serve` 之间的 wire 协议见 `../PROTOCOL.md`(同仓库 `sdks/PROTOCOL.md`)。Rust 核心新增协议变体时,SDK 自动降为 `UnknownEventMsg`,不会中断下游。