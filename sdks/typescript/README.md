# zhuye (TypeScript SDK)

让脚本项目(同样适用于 TS 服务端)直接驱动 ZhuYe Agent 的轻量 SDK。底层通过 spawn `zhuye serve` 子进程、走 JSONL stdio 协议,**零原生编译、零原生依赖**。

## 安装

```bash
npm install zhuye
```

安装即用:主包通过 optional dependencies 自动安装当前平台的平台子包
(`zhuye-darwin-arm64` / `zhuye-darwin-x64` / `zhuye-linux-x64` /
`zhuye-linux-arm64` / `zhuye-win32-x64`),内嵌对应平台的 `zhuye`
二进制,SDK 优先解析它 —— 无需自装二进制、无需配环境变量。

二进制解析优先级:显式 `options.bin` → `ZHUYE_BIN` → 平台子包内嵌
二进制 → PATH 上的 `zhuye`。当前平台不在支持矩阵(如 musl/Alpine)
时回退 PATH:从 [GitHub Releases](https://github.com/Asketisch/ZhuYe/releases)
下载,或从本仓库 `make build` 产出 `target/release/zhuye` 后以
`ZHUYE_BIN` 指向。

## 用法

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();

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

## 全局事件

`nextGlobalEvent({ timeoutMs })` 读取 `id=""` 的会话事件(`approval_request` /
`token_count` / `permission_mode_changed` 等),与 `submit()` 的按 submission
路由的事件迭代器独立。超时抛错,进程 stdout 关闭后返回 `null`;未知变体
原样返回。

```ts
const handshake = await agent.nextGlobalEvent({ timeoutMs: 5000 });
```

## 环境变量

| 变量 | 作用 |
|------|------|
| `ZHUYE_BIN` | `zhuye` 二进制路径(最高优先级覆盖;缺省用平台子包内嵌二进制)。 |
| `ZHUYE_MODEL` | 模型名(spec,例如 `mock/mock-1` 用于离线 SDK 测试)。 |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 协议

SDK 与 `zhuye serve` 之间的 wire 协议见 `../PROTOCOL.md`(同仓库 `sdks/PROTOCOL.md`)。Rust 核心新增协议变体时,SDK 自动降为 `UnknownEventMsg`,不会中断下游。

## 测试

```bash
npm install
npm test           # 跑协议层 vitest(无需 zhuye 二进制)
```

完整的端到端测试(包含 `zhuye serve` 子进程)需先在仓库根 `cargo build -p zhuye-cli`,再把 `target/debug/zhuye` 暴露到 PATH 或 `ZHUYE_BIN`。