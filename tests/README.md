# tests/ —— Reflect 全功能测试套件

本目录是仓库的**统一测试入口**,覆盖 Rust 框架层与 Python / TypeScript
两个 SDK 的全部对外功能。全程**离线**(`REFLECT_MODEL=mock`,零网络、
零 API key)+ **隔离 HOME**(mktemp,不污染真实 `~/.reflect`)。

## 快速开始

```bash
./tests/run_all.sh            # 全部(Rust + Python SDK + TS SDK)
./tests/run_all.sh rust       # 只跑 Rust 侧
./tests/run_all.sh sdk-python # 只跑 Python SDK
./tests/run_all.sh sdk-ts     # 只跑 TS SDK
```

前置:`cargo build --release -p reflect-cli`(入口脚本会自动补建)、
Python 3 + pytest、Node ≥ 18。缺依赖的批次自动 SKIP,不算失败。

## 目录结构

| 路径 | 内容 |
|------|------|
| `lib.sh` | 公共助手:隔离环境、mock provider、断言、用例统计 |
| `rust/run.sh` | Rust 套件入口(依次调下面八个) |
| `rust/workspace_tests.sh` | `cargo test --workspace` + clippy `-D warnings` + fmt + doc |
| `rust/cli_core.sh` | 核心 CLI:exec(单轮/工具/plan-mode/resume)、serve(握手/远程工具/interrupt)、session、traces、login、config、doctor、version、update |
| `rust/cli_modules.sh` | 模块 CLI:mcp、lsp、plugin、task+team、pipeline、discussion、security、workspace |
| `rust/examples_e2e.sh` | `reflect` 库 6 个 examples 冒烟(headless_run / custom_tool / multi_turn / custom_provider / hook_listener / discussion_demo) |
| `rust/cli_deep.sh` | **全 17 子命令深度使用测试**:参数矩阵 / 退出码 / 输出契约(JSONL 字段级断言),非冒烟 |
| `rust/serve_ops.sh` | **serve 协议 19 Op 全量**:逐 op 断言触发的 EventMsg 与副作用(FIFO 驱动) |
| `rust/tools_drive.sh` | **内置工具全量驱动**:mock LLM 逐工具调一遍,断言 tool_call_end 输出 |
| `rust/integrations.sh` | **集成链路**:MCP 生命周期 + in-turn 调用 / skill 激活 / notes 落盘 / bubble 审批 / 配置热重载 / plugin marketplace 全链路 |
| `sdk-python/test_sdk_real_bin.py` | Python SDK 驱动真二进制:握手/多轮/自定义工具/interrupt/close 幂等/坏二进制报错 |
| `sdk-python/test_sdk_ops.py` | **Python SDK 深度 Op 面**:非 turn ops 全量收尾(submission_closed)/ 权限模式 rollout 持久化 / MCP 审批全链路(approve)/ 孤儿回执 |
| `sdk-python/run.sh` | pytest 入口(含仓库原有 `sdks/python/tests` 协议单测) |
| `sdk-ts/real-bin.test.mjs` | TS SDK 驱动真二进制(同上覆盖面,`node --test`) |
| `sdk-ts/ops.test.mjs` | **TS SDK 深度 Op 面**(镜像 `test_sdk_ops.py`:非 turn ops / 持久化 / MCP 审批 / 孤儿回执) |
| `sdk-ts/run.sh` | node --test 入口(含仓库原有 vitest 协议单测;dist 陈旧时自动重构建) |

## 设计要点

- **mock provider**:`REFLECT_MODEL=mock` 注册零网络 mock 客户端;
  `REFLECT_MOCK_SCRIPT=<jsonl>` 可逐行脚本化每次模型回复
  (`{"type":"text",...}` / `{"type":"tool_call",...}`),实现确定性断言。
- **关闭内置 hook**:exec 默认启用 `VerificationHook`(Stop 时跑
  `cargo test`)等重 hook,单轮会拖到分钟级;测试配置统一写
  `[hooks] enabled = []`。此外 `~/.reflect/config.toml` 必须存在,
  否则 ConfigWatcher 启动即报错。
- **已知限制**(断言当前行为,防意外退化):
  - ~~`pipeline run`:plan 预设模板硬编码 `{{input.audience}}`~~(v1.6 已
    修复:planner 预设只依赖 `{{topic}}`,CLI 新增 `--input k=v` 注入
    `PipelineContext.inputs`,build_factory 从配置引导 registry 使 mock
    可离线命中);`pipeline team` 失败节点存在时退出非 0(v1.6 对齐
    `pipeline run` 的 CI 语义)。
  - `discussion run`:concurrent 模式受 SubAgentFactory 深度上限约束;
    无 provider 时 CLI 降级 `run_noop`(退出 0,Result JSON 仍含
    `outcome` / `rounds_completed`)。
- **`submission_closed` 收尾标记**(v1.3 新增协议变体):非 turn 操作
  (compact / rewind / 权限模式切换 / goal / plan 等)没有 `turn_complete`
  终态事件,SDK 的 `submit()` 迭代器此前会永久挂起。serve 在每条
  submission 的 per-turn 通道排空后发 `submission_closed`(携带该
  submission 的 id),两个 SDK 把它与 turn 终态同等视为迭代器末尾。
  `rust/serve_ops.sh` 与两个 SDK 深度用例都断言这条不变量。
- **权限模式事件路由**:`permission_mode_changed` 是**全局事件**
  (`id=""`),不进按 submission id 路由的 SDK 迭代器;深度用例改断言
  rollout JSONL 里的持久化记录(含 from/to 轨迹 + at 时间戳)。顺序
  用法下每次切换恰好落 1 条;并发在途 submission 时 core 会向每条在
  途 turn 通道广播(有意语义,客户端按 id 去重即可)。
- 与 `scripts/e2e.sh` 的关系:该脚本是 CI 冒烟;本目录是**功能覆盖
  套件**(按子命令逐条断言),两者互补。
