# zhuye (TypeScript SDK)

让 Node.js / TypeScript 项目直接驱动 [ZhuYe Agent](https://github.com/Asketisch/ZhuYe)
的官方 SDK,API 与 [Python 版](../python/README.md)同构。底层 spawn
`zhuye serve` 子进程、走 JSONL stdio 协议:**安装即用**(按平台自动安装
内嵌二进制的平台子包)、**零运行时依赖**(dev 依赖仅构建/测试工具)、
**协议 v0 冻结**(Rust 核心新增变体自动降级为 `UnknownEventMsg`,不断旧客户端)。

- Node ≥ 18,纯 ESM(`import`),开箱带 `.d.ts` 类型
- 常驻会话:一个进程 = 一个 AgentThread,多轮共享上下文
- 宿主函数直接注册为 LLM 可调用的工具(远程工具协议)
- 流式输出、审批、打断、全局事件、权限模式、计划 / 目标模式

## 安装

```bash
npm install zhuye
```

安装即用:主包通过 optional dependencies 自动安装当前平台的平台子包
(`zhuye-darwin-arm64` / `zhuye-darwin-x64` / `zhuye-linux-x64` /
`zhuye-linux-arm64` / `zhuye-win32-x64`),内嵌对应平台的 `zhuye` 二进制,
SDK 优先解析它 —— 无需自装二进制、无需配环境变量。
用 `npm ls zhuye-darwin-arm64`(按平台)可确认子包已落地。

**二进制解析优先级**(双官方 SDK 一致):

```
options.bin  →  ZHUYE_BIN  →  平台子包内嵌二进制  →  PATH 上的 zhuye
```

当前平台不在支持矩阵(如 musl/Alpine)时回退 PATH:从
[GitHub Releases](https://github.com/Asketisch/ZhuYe/releases) 下载安装包,
或从源码 `make build` 产出 `target/release/zhuye` 后以 `ZHUYE_BIN` 指向。

## 快速开始

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();   // 等 session_configured 握手完成
console.log(await agent.prompt('用一句话介绍你自己'));
await agent.close();                      // 幂等:Shutdown → 等退出 → 强杀兜底
```

离线体验用 mock 模型:`ZhuyeAgent.spawn({ env: { ZHUYE_MODEL: 'mock' } })`。

## 核心概念

```
Node 进程                                  zhuye serve 子进程
┌─────────────────────┐    JSONL stdio    ┌──────────────────────┐
│ ZhuyeAgent          │ ── Submission ──▶ │ AgentThread          │
│  后台 stdout 消费    │ ◀──── Event ───── │  (StateGraph 回合)   │
│  ├ id="" 全局队列    │                   │                      │
│  └ 按 submission id ─                   │                      │
│    路由到迭代器      │                   │                      │
└─────────────────────┘                   └──────────────────────┘
```

- **常驻会话**:`spawn()` 拉起的 serve 进程在 `close()` 前一直存活,
  多轮 `submit()` 共享完整对话上下文。
- **事件双通道**:
  - *submission 事件*:挂在提交 id 上,由 `submit()` 返回的
    `AsyncIterable` 独占消费(流式 delta、turn_complete、tool_call_end 等);
  - *全局事件*(`id=""`):token_count、permission_mode_changed、握手
    session_configured 等,由 `nextGlobalEvent()` 读取。
  两通道互不混入。远程工具请求(`tool_execution_request`)虽在 wire 上
  是全局事件,但 SDK 内部拦截并调用本地 handler,不会进入任何一个队列。
- **迭代器收尾**:`turn_complete` / `turn_aborted` / `shutdown_complete` /
  `submission_closed` 为终态事件,`for await` 在其后自然结束;连接断开
  (EOF)也会唤醒并结束所有迭代器,不会永久挂起。

## 使用指南

### 流式输出(submit + for await)

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();
for await (const ev of await agent.submit('讲个笑话')) {
  const msg = ev.msg;
  if (msg.type === 'agent_message_delta') process.stdout.write(msg.delta);
  else if (msg.type === 'turn_complete') console.log('\n[turn 结束]', msg.usage);
}
await agent.close();
```

`submit(text)` 内部把文本包成 `user_input` Op,返回**该次提交专属**的
事件 `AsyncIterable`;也可直接 `submit(op)` 发任意协议 Op(见下)。

### 一步拿全文(prompt)

```ts
const answer = await agent.prompt('北京天气如何?');
```

聚合所有 delta;`turn_complete` 正常返回,`turn_aborted` / `error` 事件
抛 `Error`(部分文本不会被当作完整答案静默返回)。

### 自定义工具(宿主函数 → LLM 工具)

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();
await agent.registerTool(
  'get_weather',
  '查询指定城市的天气',
  {                                  // JSON Schema
    type: 'object',
    properties: { city: { type: 'string' } },
    required: ['city'],
  },
  async ({ city }) => ({
    content: [{ type: 'text', text: `${city} 晴 26℃` }],
    is_error: false,
    metadata: {},
    elapsed_ms: 0,
  }),
);
console.log(await agent.prompt('上海天气怎么样?'));
```

行为约定(协议 §5):

- 模型调用工具时,serve 下发 `tool_execution_request`,SDK 异步执行
  handler 并回执;stdout 消费不阻塞,handler 里可以再调 `prompt()` /
  `submit()`。
- handler 抛异常(含同步抛)→ SDK 自动转成 `is_error=true` 的文本回执,
  LLM 看到错误信息,turn 不挂死。
- 未注册的工具被调用 → 回执错误文本(同样不挂死)。
- 等待回执上限由环境变量 `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` 控制(默认 120s),
  超时以错误收尾。

### 多轮与并发

```ts
// 多轮共享上下文
await agent.prompt('我叫小明');
await agent.prompt('我叫什么?');       // 模型能答出"小明"

// 并发提交:每次 submit 独立 id 与 AsyncIterable,可同时消费
await Promise.all([
  (async () => { for await (const e of await agent.submit('问题一')) console.log(e.msg.type); })(),
  (async () => { for await (const e of await agent.submit('问题二')) console.log(e.msg.type); })(),
]);
```

### 打断与审批

```ts
// 打断当前 turn(随后该 turn 以 turn_aborted 收尾)
await agent.interrupt();

// 审批:收到全局事件 approval_request 后回决策
const event = await agent.nextGlobalEvent({ timeoutMs: 30_000 });
if (event?.msg.type === 'approval_request') {
  await agent.approve(event.msg.request_id, 'approve');          // 或 'approve_for_session'
  // 拒绝:await agent.approve(event.msg.request_id, { deny: { reason: '太危险' } });
}
```

`decision` 透传 wire 形态,不做形状转换:`'approve'` /
`'approve_for_session'`,或 `{ deny: { reason: '...' } }`(旧版
`{type:'approve'}` 无法被服务端反序列化,勿用)。Hook 审批使用同一
decision 形态(serve 按事件种类自行分发)。

### 全局事件

```ts
const event = await agent.nextGlobalEvent({ timeoutMs: 5_000 });  // 无事件时抛超时错误
```

- 握手 `session_configured` 本身也在全局队列里,通常第一条就是它;
- stdout EOF / `close()` 后返回 `null`,重复调用安全;
- 与 submission 迭代器完全独立,未知变体原样返回。

### 会话控制 Op(submit(op))

`submit(op)` 发任意协议 Op 并返回事件 `AsyncIterable`(非 turn Op 以
`submission_closed` 收尾;可 `{ submissionId }` 显式指定 id)。Op 全集
(19 种,`zhuye` 包内同名类型):

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
| 工具 | `register_tools` / `tool_execution_response` | 通常用 `registerTool()` 封装,不必手写 |

示例:切计划模式、带 thread_settings 提交

```ts
for await (const e of await agent.submit({ type: 'set_permission_mode', mode: 'plan' })) {}
for await (const e of await agent.submit({
  type: 'user_input',
  items: [{ type: 'text', text: '调研一下' }],
  thread_settings: { effort: 'high' },
})) {}
```

## API 参考

### `ZhuyeAgent`

| 成员 | 说明 |
|---|---|
| `ZhuyeAgent.spawn(options?): Promise<ZhuyeAgent>` | 拉起 `zhuye serve`,等握手完成;二进制找不到(经 `await` 后首个调用暴露)/ 握手超时抛 `Error` |
| `ZhuyeAgent.fromStdio(stdin, stdout, handshakeTimeoutMs?)` | 测试注入:用现成 duplex 流代替 spawn |
| `registerTool(name, description, parameters, handler): Promise<void>` | 注册远程工具,任意时刻可调;静默接受,错误异步以 error 事件回报 |
| `submit(text): Promise<AsyncIterable<Event>>` | 提交用户文本,返回该提交的事件流 |
| `submit(op, opts?): Promise<AsyncIterable<Event>>` | 提交任意 Op;`opts.submissionId` 可显式指定 id |
| `prompt(text): Promise<string>` | 聚合 delta 返回全文;`turn_aborted` / `error` 抛 `Error` |
| `interrupt(childId?): Promise<void>` | 打断在飞 turn |
| `approve(approvalId, decision): Promise<void>` | 回审批决策(wire 形态透传) |
| `nextGlobalEvent(opts?): Promise<Event \| null>` | 读全局事件;`timeoutMs` 到点抛错,EOF 后返回 `null` |
| `close(): Promise<void>` | 幂等关闭:Shutdown → 等 2s → 强杀;之后所有写入静默丢弃,迭代器收尾 |
| `childProcess` (getter) | 暴露 serve 子进程句柄(测试/诊断用) |

### `ZhuyeAgentOptions`

| 字段 | 默认 | 说明 |
|---|---|---|
| `bin` | `undefined` | 二进制路径;缺省走解析优先级(见安装) |
| `cwd` | `undefined` | serve 进程工作目录 |
| `env` | `undefined` | 追加环境变量(**在 `process.env` 上合并**,如 `ZHUYE_MODEL=mock`) |
| `handshakeTimeoutMs` | `30000` | `session_configured` 握手超时 |

### 工具函数(同样从包根导出)

- `resolveBundledBinary(): string \| null` —— 解析平台子包内嵌二进制路径;
  未落地返回 `null`。
- `platformPackageName(platform?, arch?): string \| null` —— 当前平台对应的
  子包名(如 `zhuye-darwin-arm64`);不在支持矩阵返回 `null`。

### 数据类型(均从包根导出)

- `ToolOutput`:工具回执 `{content: ContentBlock[], is_error, metadata, elapsed_ms}`。
- `ContentBlock`:`{type: 'text', text: string}`。
- `RemoteToolSpec`:`{name, description, parameters(JSON Schema)}`。
- `Event`:`{id: string, msg: EventMsg}`;`id === ''` 为全局事件。
- `Op` / `EventMsg` / `UnknownEventMsg` / `Submission` / `ReviewDecision` /
  `EVENT_ID_NONE`:协议类型(见下),全部带精确字面量类型。

## 事件参考

`ev.msg.type` 的完整变体(类型定义见 `src/protocol.ts`,与 Rust
`EventMsg` 一一对应):

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
| `ask_user_question` / `ask_user_input` | 全局 | 需要用户作答 → `submit()` 回应对应 response Op |
| `permission_mode_changed` | 全局 | 权限模式切换结果 |
| `context_compacted` | submission | 压缩完成 |
| `plan_request` / `plan_ready` / `plan_approved` / `plan_rejected` / `plan_draft_updated` / `plan_step` | submission / 全局 | 计划模式事件 |
| `error` / `stream_error` | submission | 错误(`error` 会被 `prompt()` 转为异常) |
| `shutdown_complete` | submission | 关闭完成 |
| `submission_closed` | submission | 该提交通道排空(非 turn Op 的收尾标记) |
| `turn_rewound` / `config_reloaded` / `collab_*` / `mcp_server_*` / `mcp_tool_invoked` / `lsp_server_*` / `plugin_loaded` / `quota_exhausted` / `routing` | — | 其余变体见 protocol.ts |
| `UnknownEventMsg` 形态 | 任意 | 未识别变体降级(`type` 保留原值),保证前向兼容 |

## 错误处理速查

| 场景 | 表现 |
|---|---|
| 找不到二进制(spawn ENOENT) | 首个 `await`(submit/registerTool)抛 `zhuye serve failed to start: ...`,不会以 uncaught exception 打爆宿主 |
| 握手超时 | 抛 `zhuye serve handshake timed out after ...ms` |
| 模型返回 error 事件 | `prompt()` 抛 `Error: zhuye error: <code>: <message>`;`submit()` 流里自行处理 |
| turn 被打断 | `prompt()` 抛 `Error: zhuye turn aborted: ...`;`submit()` 流里见 `turn_aborted` |
| 全局事件读超时 | `nextGlobalEvent` 抛 `zhuye nextGlobalEvent timed out ...` |
| handler 抛异常 | 自动转 `is_error=true` 回执,不影响 turn;不影响 SDK |
| `close()` 后写入 | 静默丢弃(回执无处可去,不炸宿主) |

## 环境变量

| 变量 | 作用 |
|------|------|
| `ZHUYE_BIN` | `zhuye` 二进制路径(最高优先级覆盖;缺省用平台子包内嵌二进制)。 |
| `ZHUYE_MODEL` | 模型 spec(`mock/mock-1` 用于离线测试)。 |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | 远程工具等待客户端回执的上限,默认 120s。 |

## 测试

```bash
npm install
npm run typecheck    # tsc --noEmit
npm test             # 协议层 vitest(mock serve,离线)
```

mock serve 进程由测试内嵌脚本直接跑在 Node 上,不依赖真 `zhuye` 二进制 ——
完全离线,不需要网络/API key。

端到端测试(对真 `zhuye serve` 子进程的集成)在仓库 CI 里跑:构建二进制后
执行 `./tests/run_all.sh sdk-ts`。

## 协议与版本兼容

- SDK 与 `zhuye serve` 之间的 wire 协议见 [`../PROTOCOL.md`](../PROTOCOL.md)
  (线上:[GitHub](https://github.com/Asketisch/ZhuYe/blob/main/sdks/PROTOCOL.md))。
- 协议 v0 已冻结:Rust 核心新增 Op / EventMsg 变体属 additive 扩展,SDK
  遇到未识别变体降级为 `UnknownEventMsg` 原样透传,不会崩。
- SDK 版本与 Rust workspace 版本统一管理(release 流水线保证四处一致)。

## 许可证

MIT。仓库:<https://github.com/Asketisch/ZhuYe>。
