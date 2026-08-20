# reflect-agent (TypeScript SDK)

让脚本项目(同样适用于 TS 服务端)直接驱动 Reflect Agent 的轻量 SDK。底层通过 spawn `reflect serve` 子进程、走 JSONL stdio 协议,**零原生编译、零原生依赖**。

## 安装

```bash
npm install reflect-agent
```

需要本机有 `reflect` 二进制在 PATH(或通过 `REFLECT_BIN` 指向)。从 [Releases](https://cnb.cool/...) 下载,或从本仓库 `make build` 产出 `target/release/reflect`。

## 用法

```ts
import { ReflectAgent } from 'reflect-agent';

const agent = await ReflectAgent.spawn();

// 1) 注册客户端自定义工具(LLM 调用时由本地函数返回结果)
await agent.registerTool(
  'get_weather',
  '查询指定城市的天气',
  {
    type: 'object',
    properties: { city: { type: 'string' } },
    required: ['city'],
  },
  async ({ city }) => ({
    content: [{ type: 'text', text: `${city} 今天晴 26℃` }],
    is_error: false,
    metadata: {},
    elapsed_ms: 0,
  }),
);

// 2) 提交 prompt,流式接收事件
const text = await agent.prompt('北京天气如何?');
console.log(text); // "北京 今天晴 26℃"

// 3) 也可手动遍历事件:
for await (const ev of await agent.submit('第二轮')) {
  if (ev.msg.type === 'agent_message_delta') process.stdout.write(ev.msg.delta);
  if (ev.msg.type === 'turn_complete') break;
}

await agent.close();
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `REFLECT_BIN` | `reflect` 二进制路径(默认搜索 PATH)。 |
| `REFLECT_MODEL` | 模型名(spec,例如 `mock/mock-1` 用于离线 SDK 测试)。 |
| `REFLECT_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 协议

SDK 与 `reflect serve` 之间的 wire 协议见 `../PROTOCOL.md`(同仓库 `sdks/PROTOCOL.md`)。Rust 核心新增协议变体时,SDK 自动降为 `UnknownEventMsg`,不会中断下游。

## 测试

```bash
npm install
npm test           # 跑协议层 vitest(无需 reflect 二进制)
```

完整的端到端测试(包含 `reflect serve` 子进程)需先在仓库根 `cargo build -p reflect-cli`,再把 `target/debug/reflect` 暴露到 PATH 或 `REFLECT_BIN`。