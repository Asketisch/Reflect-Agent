# 烛夜（ZhuYe）

烛夜是嵌进自己产品里的 agent harness。同一套 Rust 引擎可以用库链接进进程，也可以用 `zhuye exec` 跑一轮、用 `zhuye serve` 维持一个 stdio JSONL 会话，再由 [Python](sdks/python/zhuye/agent.py) / [TypeScript](sdks/typescript/src/agent.ts) SDK 驱动。本仓库 `zhuye` 二进制的子命令枚举里没有 TUI。工作区 [Cargo.toml](Cargo.toml) 的注释把交互界面指到 [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI)，那不是运行时跳转。

日常在终端里写代码，用 Claude Code、Codex、OpenCode 或 Pi。要把引擎嵌进自己的服务、脚本或界面，用烛夜。

English: [README_EN.md](README_EN.md).

## 和四家差在哪

四家的循环都差不多：读上下文、调模型、跑工具、再决定停不停。差别在你买到的取舍。

| 取舍 | Claude Code | Codex | OpenCode | Pi | 烛夜 |
| --- | --- | --- | --- | --- | --- |
| 产品形态 | 坐进终端里用的编程助手，提示词按 Claude 来写 | 坐进终端里用的编程助手，命令默认关进操作系统沙箱，模型偏 OpenAI | 开源终端产品，可换 provider，客户端和常驻服务分开 | 极小循环，内置四个工具，其余用 TypeScript 扩展自己加 | 引擎。库进程内链接，或 `zhuye serve` 的 stdio JSONL。本二进制没有 TUI 子命令 |
| 换模型 | 换模型仍走 Claude 那套提示词 | 换模型仍走 OpenAI 那套提示词 | 换 endpoint，提示词仍是一套 agent 文案 | 一套极短提示词打所有模型 | 内嵌提示词按 `default` → 厂商 → 精确模型名叠加。主对话有 fallback 链；压缩只用 compact primary |
| 多 agent | 再开一个子会话 | 再开一个子会话 | 再开一个子会话 | 扩展里再开一个子会话 | 讨论、DAG 流水线、目标模式是引擎原语，主循环保持原样 |
| 长上下文 | 会话变长后做压缩 | 会话变长后做压缩 | 会话变长后做压缩 | 几乎不在循环外再加脚手架 | 先在本地截断，仍超预算才调摘要模型。压缩触发后重读活跃文件；子代理登记和笔记每轮注入 |
| 隔离 | 内核沙箱，网络按域名放行 | 内核沙箱，工作区可写时网络默认关闭 | 权限是确认框；要隔离就自己放进容器 | 默认不审批、不沙箱 | `bash` 默认进入 OS 沙箱，后端不可用就失败，不退回裸 shell。macOS 默认拒绝网络；Linux 只限制写 |

下面三节是烛夜这一列的依据。

### 按模型换提示词，再按角色换模型

提示词库把内嵌文案叠成三层：`default`，然后是厂商（claude、gpt、gemini、deepseek、kimi、qwen、grok、glm、minimax），再然后是精确模型名。工作区 `.zhuye/prompts/` 盖过 `~/.zhuye/prompts/`，文件里只写要改的 section，缺的回落到内嵌默认。见 [`library.rs`](crates/abilities/zhuye-prompt/src/library.rs)。

路由只有主对话把 `Role::Main` 的 primary、`fallbacks` 和凭据 cooldown 串成一条链。压缩在构造摘要器时拷贝 `Role::Compact` 的 primary，调用时只对这一个 spec 在凭据池里切换，不读 compact 的 `fallbacks`。`Role::Subagent` 没有调用方；子代理用启动时的主模型，或 `[subagent_providers]` 的独立 registry。见 [`RoutingPolicy`](crates/abilities/zhuye-llm/src/policy.rs)。

### 讨论、流水线、目标

讨论按 TOML 里的角色跑，顺序或并发均可。`consensus_window` 决定多少轮内所有参与者都报过共识才算达成，`max_rounds` 是硬上限。打开 `judge` 后，每轮由独立模型做结构化裁决，`agreed` 才算共识，参与者自报的 Consensus 只作为参考；默认关闭，沿用自报。见 [`DiscussionConfig`](crates/orchestration/zhuye-discussion/src/models.rs)。

```bash
zhuye discussion run -c crates/orchestration/zhuye-discussion/examples/discussion.toml
```

没有可用 provider 时，讨论状态机仍会跑完并退回空 transcript，用来确认命令本身。要真的发言，需要配置 provider。

流水线是声明式 DAG。失败节点的下游在 `abort` 和 `continue_collect` 下都标 `skipped`，没有依赖的旁支照跑。两种策略都不提前结束循环，差别是总状态：`failed` 或 `partial`。仓库里的示例是 plan → prd → exec → verify，每个节点对应一个已存在的 team。见 [`zhuye-pipeline`](crates/orchestration/zhuye-pipeline/src/lib.rs) 和 [`config.example.toml`](crates/orchestration/zhuye-pipeline/examples/config.example.toml)。

```bash
zhuye pipeline run \
  -c crates/orchestration/zhuye-pipeline/examples/config.example.toml \
  --topic "build a CLI todo app"
```

目标模式在回合结束时做校验，不往状态图里加节点。客户端提交 `EnterGoalMode` 之后，每一轮结束都做一次 LLM 自校验，还可以再加一条命令，两者都通过才算完成。同一个 blocker 连续 3 次才记为卡住；token 预算默认 500000，用尽即停。没完成时，续作文本推进 steering 队列，等下一条用户输入合并进来，引擎不会自己再开一轮。见 [`GoalController::on_turn_end`](crates/orchestration/zhuye-goal/src/controller.rs)，合并点在 [`submission_loop.rs`](crates/runtime/zhuye-core/src/submission_loop.rs)。`zhuye serve` 的一行提交：

```json
{"id":"1","op":{"type":"enter_goal_mode","goal":"tests pass","verify_command":"cargo test -p zhuye-core --lib","token_budget":500000}}
```

### 先本地压缩，再按触发条件灌回状态

压缩器先估算 token。没到阈值就不动。到了先做 `microcompact`：保留 system 和第一条 user，旧工具结果换成占位，去掉 thinking。还不够再做 `smart_prune`：按工具类型截断，并丢掉最旧的非保留消息。仍然超过目标才调用摘要模型。摘要用的是构造时拷下的 compact primary，可以和主对话不是同一个模型。见 [`Compactor`](crates/abilities/zhuye-compact/src/strategy.rs)。

活跃文件只在发生过非 Noop 压缩时重读。已完成的子代理登记和会话笔记每个 `pre_loop` 都注入，再由 `model_call` 追加进消息。笔记是最多 30 条的 FIFO，追加时不再调一次模型，并落到 JSONL，和长篇 `MEMORY.md` 分开。见 [`zhuye-recovery`](crates/abilities/zhuye-recovery/src/lib.rs) 和 [`zhuye-notes`](crates/abilities/zhuye-notes/src/lib.rs)。

### 沙箱按平台说清楚

`bash` 默认跑在 OS 沙箱里，`strict` 为开。macOS 用 Seatbelt，默认拒绝网络，工作区可写。Linux 用 Landlock，限制的是写；内核不支持时命令失败，而不会退回裸 `sh -c`。Linux 上的「网络关闭」没有对应的内核规则，状态输出也不会写成网络已拒绝。见 [`os_sandbox.rs`](crates/resources/zhuye-sandbox/src/os_sandbox.rs)。

## 三条可以马上试的路径

引擎仓库要求 Rust 1.85。二进制：

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` 是显式离线模型，不会在缺少 API key 时被静默换上。真实模型设置 `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY`，或在 `~/.zhuye/config.toml` 里配置 provider。事件写 stdout，日志写 stderr。

进程内链接（`mock` 同样免 key）：

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

`build()` 会装上压缩、记忆、技能和会话记录。见 [`Zhuye::builder`](crates/runtime/zhuye/src/lib.rs)。

宿主语言里的工具留在宿主进程。SDK 拉起 `zhuye serve`（可用 `ZHUYE_BIN` 指定二进制），`register_tool` 只把名字和 JSON Schema 交给引擎；模型要调用时，引擎发 `tool_execution_request`，函数在 Python 或 TypeScript 里执行，再把结果回执。

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather",
    "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴")],
        is_error=False,
    ),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

Python 包在 [`sdks/python`](sdks/python)（仅标准库），TypeScript 包在 [`sdks/typescript`](sdks/typescript)。

## CLI 一览

`zhuye` 二进制共 19 个子命令，按用途分组：

| 命令 | 用途 |
|------|------|
| `exec` | 单轮 headless 对话，Event 以 JSONL 流写 stdout |
| `serve` | 常驻 stdio JSONL 会话服务，SDK 的协议入口 |
| `discussion` | 按 TOML 配置运行多 agent 讨论 |
| `pipeline` | 运行声明式 DAG 流水线 |
| `task` | 结构化任务与团队管理 |
| `login` | 把 provider 凭据写入 `~/.zhuye/config.toml` |
| `models` | 列出 / 挑选 provider 当前可用模型，清单实时取自接口 |
| `mcp` | MCP server 配置的增删与连通测试 |
| `lsp` | LSP server 配置查看 |
| `plugin` | 插件安装 / 启用 / 禁用 / 卸载 |
| `config` | 查看 / 编辑 `~/.zhuye/config.toml`，写入后热重载 |
| `session` | 持久化会话的列出 / 查看 / 删除 / fork / 跨会话搜索 |
| `traces` | 本地 model-io 调用记录 |
| `security` | `cargo audit` 安全扫描 |
| `workspace` | workspace 的 git clone / sync |
| `doctor` | 环境自检（config / rollout / mcp / rustc） |
| `init` | 生成 AGENTS.md 项目说明 |
| `update` | 打印升级说明（不联网） |
| `version` | 打印版本号 |

参数细节看 `zhuye <命令> --help`。会话续跑用 `zhuye exec --resume`，按序号（`-r 2`）或完整 UUID 均可。

## 配置要点

配置在 `~/.zhuye/config.toml`（`ZHUYE_HOME` 可整体挪走）：

```toml
[active]
provider = "openai"
# 可选：在 config 层裁剪主 agent 工具面，白名单 ∩ / 黑名单 −，
# 与 .zhuye/agents/*.md 的 tools / disallowed_tools 同语义：
# tools = ["read", "grep"]
# disallowed_tools = ["bash", "write", "web_search"]

[openai]
api_key = ""                                  # 免 key 端点可留空
base_url = "https://api.kilo.ai/api/gateway"  # 已含版本段（/v1、/v4）时不再追加 /v1
model = "free"                                # 免费模型：运行时从清单里挑一个
temperature = 0.3                             # 可选：默认采样温度（请求侧显式值优先）
max_tokens = 4096                             # 可选：默认输出上限
```

`model = "free"` 配套模型发现：`zhuye models list --free-only` 看当前免费模型，`zhuye models pick --free-only` 挑一个。清单实时取自 provider 接口，不写死模型 id。实模型演练见 [`FREE_MODEL_PRACTICE.md`](docs/FREE_MODEL_PRACTICE.md)。

`zhuye config set` 写入后 ConfigWatcher 热重载，不用重启进程。

## 环境变量

| 变量 | 说明 |
|------|------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | 对应 provider 凭据，起步二选一 |
| `OLLAMA_HOST` / `OLLAMA_API_KEY` | Ollama base_url（完整 URL 含 scheme）/ Bearer |
| `ZHUYE_PROVIDER` / `ZHUYE_MODEL` | 强制 provider / 覆盖模型规格；`ZHUYE_MODEL` 带已知 provider 前缀（如 `openai/m1`）会剥掉前缀，`=mock` 供离线运行 |
| `ZHUYE_MOCK_SCRIPT` | mock 脚本，JSONL 文件路径，每行一次模型调用（text / tool_call / tool_call_delta） |
| `ZHUYE_HOME` | 数据目录（默认 `~/.zhuye`）；config、prompts、session notes 等全部状态跟随该目录 |
| `ZHUYE_BIN` | SDK 定位 `zhuye` 二进制（默认 PATH 查找） |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | serve 远程工具等回执上限（默认 120 秒） |
| `ZHUYE_AUTO_COMPACT_INPUT_TOKENS` | 压缩触发阈值（默认 10,000） |
| `ZHUYE_MAX_ITERATIONS` | 图执行最大迭代数安全阀 |
| `ZHUYE_SANDBOX_STRICT` 等 | 沙箱档位开关（`STRICT` / `OS_LEVEL` / `WRITABLE`） |
| `RUST_LOG` | tracing filter（默认 `warn,zhuye=info`） |

## 文档

| 文档 | 内容 |
|------|------|
| [`docs/architecture.md`](docs/architecture.md) | 架构详解：六层职责、目录结构、核心数据流、设计决策 |
| [`docs/sdk.md`](docs/sdk.md) | SDK 接入指南：serve 模式、远程工具流、离线联调 |
| [`sdks/PROTOCOL.md`](sdks/PROTOCOL.md) | serve wire 协议规范 |
| [`docs/prompts.md`](docs/prompts.md) | 提示词库与分层覆盖 |
| [`docs/tools-and-hooks.md`](docs/tools-and-hooks.md) | 内置工具与 hooks |
| [`docs/plugins.md`](docs/plugins.md) | 插件开发：manifest、capability、marketplace |
| [`docs/PLAN_MODE.md`](docs/PLAN_MODE.md) | Plan mode |
| [`docs/sanitize.md`](docs/sanitize.md) | 输出净化 |
| [`docs/FREE_MODEL_PRACTICE.md`](docs/FREE_MODEL_PRACTICE.md) | 免费模型实跑实践 |
| [`docs/release.md`](docs/release.md) | SDK 发布（PyPI / npm） |
| [`AGENTS.md`](AGENTS.md) | 开发指南与项目约定 |

## 开发

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # 能力覆盖层（Python 打真二进制）
./scripts/e2e.sh                                      # 端到端冒烟
```

examples 与 e2e 用 `ZHUYE_MODEL=mock` 走离线 mock provider，不需要 API key。

## 边界

交互界面不在这个仓库。终端里的完成度是 Claude Code 的取舍；四个工具和极短提示词是 Pi 的取舍。烛夜在这两件事上没有对应的产品表面。

Linux 沙箱限制写，不切断网络。macOS Seatbelt 才会在 `allow_network = false`（默认）时拒绝网络。

同一个父会话里同时在途的子代理最多 16 个。常量名是 `MAX_DEPTH`，计的是 in-flight：子任务结束就释放名额，不是嵌套深度，也不是累计 spawn 次数。见 [`zhuye-subagent`](crates/orchestration/zhuye-subagent/src/lib.rs)。

这里不比较 token 消耗或任务通过率。那些公开评测没有烛夜。

## 引擎里还有这些

MCP、LSP、hooks、权限模式、按 thread id 恢复会话（`zhuye exec --resume`）、把对话回退到某一轮（`Op::Rewind`）、以及每次模型调用的本地 model-io 记录（`zhuye traces`），四家都有同类物。实现分别在 [`zhuye-mcp`](crates/integrations/zhuye-mcp)、[`zhuye-lsp`](crates/integrations/zhuye-lsp)、[`zhuye-hooks`](crates/abilities/zhuye-hooks)、[`zhuye-permissions`](crates/resources/zhuye-permissions)、[`zhuye-exec`](crates/runtime/zhuye-exec)、[`Op`](crates/protocol/zhuye-protocol/src/op.rs)、[`zhuye-telemetry`](crates/resources/zhuye-telemetry)。

## 许可

引擎与 CLI 为 [Apache-2.0](LICENSE)。Python SDK 与 TypeScript SDK 各自的包清单为 MIT。仓库：<https://github.com/Asketisch/ZhuYe>。
