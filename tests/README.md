# tests/ —— Reflect 全功能测试套件

本目录是仓库的**统一测试入口**,覆盖 Rust 框架层与 Python / TypeScript
两个 SDK 的全部对外功能。全程**离线**(`REFLECT_MODEL=mock`,零网络、
零 API key)+ **隔离 HOME**(mktemp,不污染真实 `~/.reflect`)。

## 快速开始

```bash
./tests/run_all.sh            # 全部(功能覆盖 + Rust 全套 + 双 SDK)
./tests/run_all.sh functional # 功能覆盖门禁(CI 用):能力覆盖层 + Rust 参数面 + 双 SDK 绑定层
./tests/run_all.sh functional-layer  # 只跑能力覆盖层
./tests/run_all.sh live       # 实模型演练层(真实模型;默认 SKIP,需 REFLECT_LIVE=1)
./tests/run_all.sh rust       # 只跑 Rust 侧(含 cargo test / clippy / fmt / doc)
./tests/run_all.sh sdk-python # 只跑 Python SDK 绑定层
./tests/run_all.sh sdk-ts     # 只跑 TS SDK 绑定层
```

前置:`cargo build --release -p reflect-cli`(入口脚本会自动补建)、
Python 3 + pytest、Node ≥ 18。缺依赖的批次自动 SKIP,不算失败。
审批类用例另需 `cargo build -p reflect-mcp`(产出 `target/debug/mock_mcp_server`)。

## 四层分工

> 分层的理由:本仓是 **agent 基座**,对外提供 Python / TS 两套 SDK。
> 服务端(Rust)只有一份,客户端有两份 —— 所以**服务端语义只测一遍**,
> 客户端行为各测一遍。

| 层 | 位置 | 测什么 | 依赖 SDK? |
|------|------|--------|-----------|
| **能力覆盖层**(主) | `tests/functional/` | **Rust 代码在真产物上的行为**:起 `reflect serve` 子进程、走原始 JSONL wire、断言事件字段 | ❌ 不依赖 |
| CLI 参数面 | `tests/rust/*.sh` | 17 个子命令的**参数矩阵**与输出契约(bash) | ❌ 不依赖 |
| SDK 绑定层 | `tests/sdk-python/`、`tests/sdk-ts/` | 两个**客户端自己**的行为:迭代器收尾、handler 调度、回执形状、close 语义 | ✅ 依赖 |
| SDK 单测 | `sdks/python/tests/`、`sdks/typescript/tests/` | SDK 纯逻辑(mock serve 驱动),随包发布给外部用户 | ✅ 依赖 |
| **实模型演练层**(可选) | `tests/live/` | **真实模型**下的引擎行为:协议形状 / 终态 / usage / 工具成对。模型从 provider 清单接口**实时获取** | ❌ 不依赖 |

**判据**:
- 断言「服务端发什么 / 落盘什么」→ **能力覆盖层**;
- 断言「客户端自己怎么反应」→ **SDK 绑定层**;
- 断言「某个 CLI 参数组合的输出契约」→ bash 参数面;
- **新增能力断言只落能力覆盖层一处** —— 不要在 SDK 测试里重复写协议/能力断言。

> 为什么要把能力面与 SDK 面分开:SDK 自身的 bug 会把 Rust 的 bug 遮住。
> 实例:Python SDK 的 `close()` 从不发出 shutdown op(见 `COVERAGE.md`
> 「已知缺陷」D1)—— 若能力面经由 SDK 驱动,这类差异会被误读成"服务端行为"。

## 目录结构

| 路径 | 内容 |
|------|------|
| `lib.sh` | 公共助手:隔离环境、mock provider、断言、用例统计 |
| `functional/conftest.py` | 能力覆盖层设施:原始 wire 会话(起 serve / 逐行收发 JSONL)、CLI 调用、rollout 读取 |
| `functional/test_serve.py` | 协议面:基础通信 / 并发路由 / 19 Op 会话状态面 |
| `functional/test_tools.py` | 工具面:内置工具 / 远程工具 / 审批链路(approve + deny) |
| `functional/test_recovery.py` | 重连恢复(`--resume` / `-c` / `-r` / 崩溃 / EOF)+ 容错(坏行 / 撞名 / 超时) |
| `functional/test_persistence.py` | 持久化面:rollout 记录集 / 权限模式轨迹 / rewind 截断 / 会话索引 |
| `functional/test_orchestration.py` | 编排面:子代理 / 讨论 / 任务团队 / 流水线 / goal 模式 |
| `functional/test_cli.py` | CLI 能力面(行为断言;参数矩阵仍在 bash) |
| `functional/run.sh` | 能力覆盖层入口 |
| `rust/run.sh` | Rust 套件入口(依次调下面八个;`REFLECT_SKIP_WORKSPACE=1` 跳过第 1 步) |
| `rust/workspace_tests.sh` | `cargo test --workspace` + clippy `-D warnings` + fmt + doc |
| `rust/cli_core.sh` | 核心 CLI:exec(单轮/工具/plan-mode/resume)、serve(握手/远程工具/interrupt)、session、traces、login、config、doctor、version、update |
| `rust/cli_modules.sh` | 模块 CLI:mcp、lsp、plugin、task+team、pipeline、discussion、security、workspace |
| `rust/examples_e2e.sh` | `reflect` 库 6 个 examples 冒烟(headless_run / custom_tool / multi_turn / custom_provider / hook_listener / discussion_demo) |
| `rust/cli_deep.sh` | **全 17 子命令深度使用测试**:参数矩阵 / 退出码 / 输出契约(JSONL 字段级断言),非冒烟 |
| `rust/serve_ops.sh` | **serve 协议 19 Op 全量**:逐 op 断言触发的 EventMsg 与副作用(FIFO 驱动) |
| `rust/tools_drive.sh` | **内置工具全量驱动**:mock LLM 逐工具调一遍,断言 tool_call_end 输出 |
| `rust/integrations.sh` | **集成链路**:MCP 生命周期 + in-turn 调用 / skill 激活 / notes 落盘 / bubble 审批 / 配置热重载 / plugin marketplace 全链路 |
| `sdk-python/conftest.py` | SDK 绑定层共享 fixture(隔离 HOME / spawn 选项 / drain 墙钟兜底) |
| `sdk-python/test_sdk_real_bin.py` | Python SDK 绑定层:握手/多轮 prompt/远程工具本地执行/非 turn op 收尾/interrupt/close 幂等/坏二进制/handler 异常回执 |
| `sdk-python/run.sh` | pytest 入口(含仓库原有 `sdks/python/tests` 协议单测) |
| `sdk-ts/real-bin.test.mjs` | TS SDK 绑定层(5 条:证明 TS SDK 仍能与真服务端对话) |
| `sdk-ts/run.sh` | node --test 入口(含仓库原有 vitest 协议单测;dist 陈旧时自动重构建) |
| `live/conftest.py` | 实模型层设施:模型清单拉取 + 能力探测 + 隔离环境 |
| `live/liveutil.py` | 实模型层助手:换会话重试 / 限流判定 / 协议形状断言 |
| `live/test_live_basic.py` | 基础面:握手 / 单轮 / usage / 多轮 |
| `live/test_live_tools.py` | 工具面:内置工具调用 / 未注册工具 / 工具输出 |
| `live/test_live_persistence.py` | 持久化 + 容错面:rollout 落盘 / 多轮累积 / 限流可观测 |
| `live/run.sh` | 实模型层入口(默认 SKIP) |

## 设计要点

- **mock provider**:`REFLECT_MODEL=mock` 注册零网络 mock 客户端;
  `REFLECT_MOCK_SCRIPT=<jsonl>` 可逐行脚本化每次模型回复
  (`{"type":"text",...}` / `{"type":"tool_call",...}`),实现确定性断言。
- **关闭内置 hook**:exec 默认启用 `VerificationHook`(Stop 时跑
  `cargo test`)等重 hook,单轮会拖到分钟级;测试配置统一写
  `[hooks] enabled = []`。此外 `~/.reflect/config.toml` 必须存在,
  否则 ConfigWatcher 启动即报错。
- **墙钟兜底**:所有"等待事件"的断言都有超时上限(30s),回归时**断言失败**
  而不是拖死整个套件。
- **`submission_closed` 收尾标记**(v1.3 新增协议变体):非 turn 操作
  (compact / rewind / 权限模式切换 / goal / plan 等)没有 `turn_complete`
  终态事件,SDK 的 `submit()` 迭代器此前会永久挂起。serve 在每条
  submission 的 per-turn 通道排空后发 `submission_closed`(携带该
  submission 的 id),两个 SDK 把它与 turn 终态同等视为迭代器末尾。
  `tests/functional/test_serve.py` 与两个 SDK 绑定层都断言这条不变量。
- **权限模式事件路由**:`permission_mode_changed` 是**全局事件**
  (`id=""`),不进按 submission id 路由的迭代器;能力覆盖层用
  `global_events()` 收集,SDK 侧因无公开读取口只能读私有队列
  (见 `COVERAGE.md`「已知缺陷」D4)。
- **实模型层是独立层**:默认 SKIP(不发起网络请求),不进 CI 门禁;模型
  **从 provider 接口实时获取**(不写死 id —— 免费模型集合随时变);断言只到
  **协议形状**,不断言回答文本;上游限流(429)判为**跳过**而非失败。
  详见 [`live/README.md`](live/README.md) 与
  [`docs/FREE_MODEL_PRACTICE.md`](../docs/FREE_MODEL_PRACTICE.md)。
- **三个 id 互不相同**(易踩坑):`session_configured.session_id`、
  rollout 内 `session_meta.session_id`、**会话文件名**里的 thread id 是
  三个不同的 uuid。`--resume` 的参数必须用**文件名**里的 id。
- 与 `scripts/e2e.sh` 的关系:该脚本是 CI 冒烟;本目录是**功能覆盖
  套件**(按能力域逐条断言),两者互补。
