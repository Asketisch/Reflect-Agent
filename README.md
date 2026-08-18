# Reflect

> Rust 编写的 AI agent 运行时:4 节点 StateGraph 引擎、23 内置工具、8 hook 事件、
> 多 LLM provider、MCP / LSP 集成、JSONL rollout 持久化,产出单一 `reflect` CLI 二进制。

**0.0.1** · 33 crate · **2200+ tests** · Apache-2.0

## 特性

| 能力 | 实现 |
|------|------|
| 4 节点 StateGraph | PreLoop → ModelCall → ToolExec → CheckStop 自循环 |
| OpenAI + Anthropic + Ollama | SSE 流式、prompt caching、extended thinking、本地 NDJSON |
| 23 内置工具 | bash / read / write / edit / grep / glob / web_fetch / web_search / notebook_edit / image_view / task 工具 + Plan mode 控制面等 |
| 8 Hook 事件 × 7 决策 | PreToolUse / PostToolUse / PostToolUseFailure / Stop / SessionStart + 3 个 task 生命周期事件 |
| 4 层上下文压缩 | microcompact → smart_prune → LLM summarize(escalation) |
| 子代理 | Tool-per-Agent,嵌套深度 ≤ 3 |
| 多 Agent 编排 | discussion(顺序/并发)、task / team、DAG pipeline、goal 模式 |
| 持久化 | JSONL rollout + resume + session 索引 + LLM traces 记录 |
| MCP | stdio / streamable-http 两种 transport + tool prefix + reload diff |
| LSP | LSP server 配置管理与集成 |
| 插件 | 安装 / 启用 / 禁用 / 卸载 + marketplace |
| 权限与沙箱 | permissions 规则引擎 + 多级 sandbox |
| Plan mode | `reflect exec --plan-mode` + `EnterPlanMode` / `ExitPlanMode` 工具 |
| 配置 | 统一 TOML + notify 热重载 + CLI 子命令 |
| CLI | 单一 `reflect` 二进制 + 16 个子命令 |
| Python 绑定 | reflect-py(PyO3) |
| 测试 | 2200+ tests · wiremock · insta snapshot · 跨平台 CI |

## 快速开始

```bash
# 1. 构建(需要 Rust 1.85+,rust-toolchain.toml 已锁定)
git clone https://cnb.cool/Demon1019/Reflect-Agent && cd Reflect-Agent
cargo build --release            # 或 make build

# 2. 设置 API key(三选一)
export OPENAI_API_KEY=sk-...
# 或
export ANTHROPIC_API_KEY=sk-ant-...
# 或写入 config
./target/release/reflect login --provider anthropic --api-key sk-ant-...

# 3. headless 单轮对话(JSONL Event 流到 stdout,日志走 stderr)
./target/release/reflect exec "what is 2+2?" | jq -c '.msg.type'

# 4. 续上次 session
./target/release/reflect exec -c

# 5. Plan mode —— 只读调研模式
./target/release/reflect exec --plan-mode "summarize the auth module"

# 6. Rust 库集成
cargo run -p reflect --example headless_run -- "say hi"

# 7. 交互式 TUI —— 使用独立仓库的产物
#    https://cnb.cool/Demon1019/Reflect-CLI
```

离线开发:examples 与 e2e 脚本用 `REFLECT_MODEL=mock` 跳过真实 LLM 网络调用。

## 架构

### Workspace 分层(依赖方向自上而下)

| 层 | Crate | 职责 |
|----|-------|------|
| `protocol/` | reflect-protocol | Submission / Op / EventMsg / Item + RolloutRecorder trait。所有层的公共契约 |
| `abilities/` | llm, tools, hooks, skills, memory, agent-def, prompt, compact, recovery, notes, sanitize | 能力原语:ModelClient trait + providers、Tool trait + 内置工具、HookEngine、压缩 escalation 等 |
| `resources/` | config, permissions, plugin, rollout, telemetry, sandbox, ast | 配置热重载、权限规则、插件 lifecycle、JSONL 持久化、tree-sitter 代码搜索 |
| `orchestration/` | subagent, discussion, task, pipeline, goal | 多 agent 编排:子代理工厂(深度≤3)、讨论编排、任务/团队、DAG 流水线、目标模式 |
| `integrations/` | mcp, lsp, integration, stream | MCP 客户端(stdio/streamable-http)、LSP、流式会话后端 |
| `runtime/` | core, exec, cli, reflect, py | AgentThread + StateGraph、headless、CLI 入口、lib facade(60+ re-exports + Builder)、PyO3 绑定 |

### 核心数据流

- `AgentThread`(reflect-core)消费 `Submission`(mpsc 通道),`submission_loop` 驱动
  4 节点 `StateGraph`:**PreLoop → ModelCall → (ToolExec → PreLoop)\* → CheckStop**。
  模型不再发工具调用时经 CheckStop 结束 turn(Stop hook 可否决强制续跑)。
- `NodeContext` 携带回合级依赖:model registry、RoutingPolicy、HookEngine、
  ToolExecutionQueue、event 通道、CancellationToken、approval gates。
- 客户端(TUI / exec / lib)统一通过 Submission/Event 协议通信;headless 模式
  Event 以 JSONL 流写 stdout、tracing 日志走 stderr(`reflect exec | jq` 不被破坏)。

### 关键设计决策

- **Protocol v0 已冻结**:新增 Op/EventMsg 变体 = non-breaking,不可改动既有变体
- **JSONL stdout,日志 stderr**:`reflect exec | jq` 不破坏
- **ToolError 定义在 reflect-protocol**:打破 tools ↔ hooks 循环依赖
- **StateGraph 用 enum + match**:4 节点固定结构,不引入 petgraph
- **错误处理**:库 crate 用 `thiserror`,应用层(exec/cli)用 `anyhow`

## CLI 子命令

| 子命令 | 用途 |
|--------|------|
| `reflect exec` | 单轮 headless,JSONL Event 流到 stdout(支持 `-c` / `-r N` / `--resume <uuid>` / `--agent <name>` / `--plan-mode`) |
| `reflect discussion` | 多 Agent 讨论编排(`run -c <toml>` / `ls`) |
| `reflect login` | 写 provider 凭据到 `~/.reflect/config.toml` |
| `reflect mcp` | MCP server 配置(`ls` / `add` / `remove` / `test` / `show`) |
| `reflect config` | 配置读写(`show` / `set` / `unset` / `edit` / `ls`) |
| `reflect session` | 持久化 session 管理(`ls` / `show` / `rm` / `fork` / `rename` / `export`) |
| `reflect traces` | 本地 LLM 调用记录查看(`ls` / `show`,数据在 `~/.reflect/traces/model-io/`) |
| `reflect doctor` | best-effort 环境自检(支持 `--check-network`) |
| `reflect plugin` | 插件安装 / 列出 / 启用 / 禁用 / 卸载 + marketplace |
| `reflect lsp` | LSP server 配置管理 |
| `reflect task` | 结构化任务 + 团队管理(TaskCreate / TaskList / TeamCreate 等) |
| `reflect pipeline` | DAG 流水线(team-plan → team-prd → team-exec → team-verify) |
| `reflect security` | 安全扫描(cargo audit) |
| `reflect workspace` | workspace 的 git clone / sync |
| `reflect update` | 打印升级说明(当前不联网) |
| `reflect version` | 打印版本号 |

### Resume 旗标

`-c` / `-r N` / `--resume <uuid>` 三选一,由 clap group 互斥:

```bash
reflect exec -c "继续上次的工作"         # 续最近 session
reflect exec -r 2 "在第二条上追问"       # 按序号
reflect exec --resume <uuid>             # 按 UUID
```

### 典型工作流

```bash
# 首次安装
reflect login --provider anthropic --api-key sk-ant-...
reflect exec "hello"

# 接入 MCP filesystem server
reflect mcp add filesystem --command npx \
                          --args "-y" \
                          --args "@modelcontextprotocol/server-filesystem" \
                          --args "$HOME"
reflect mcp test filesystem

# 切换 model
reflect config set anthropic.model claude-3-haiku-20240307
# ConfigWatcher 自动热重载

# 排查
reflect doctor --check-network
RUST_LOG=debug reflect exec "hello" 2>debug.log

# 续 session
reflect session ls
reflect exec -c "继续上次"
```

## Plan Mode

Plan mode 让用户在 agent 跑写工具 (`bash` / `write` / `edit`) 之前先生成完整计划:
`PlanModeGate` hook blanket-deny 写工具(read / grep / glob 等只读工具可用),
agent 调研完成后调 `ExitPlanMode` 产出计划并退出 Plan 模式。

```bash
reflect exec --plan-mode "refactor X"   # headless 调研(只读)
```

交互式场景(TUI)下的 `/plan` slash 命令与 plan approval modal 由
[Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI) 仓库提供。

## Examples

| 示例 | 演示 | 命令 |
|------|------|------|
| `headless_run` | 最小单轮 prompt | `cargo run -p reflect --example headless_run -- "..."` |
| `custom_tool` | 自定义 Tool 实现 | `cargo run -p reflect --example custom_tool` |
| `multi_turn` | 同一 agent 多轮 history | `cargo run -p reflect --example multi_turn` |
| `custom_provider` | 离线 MockLlmClient | `cargo run -p reflect --example custom_provider` |
| `hook_listener` | TokenUsage + DangerousCommand hook | `cargo run -p reflect --example hook_listener` |
| `discussion_demo` | 3-agent 讨论演示 | `cargo run -p reflect --example discussion_demo` |

## 环境变量

| 变量 | 必需 | 说明 |
|------|------|------|
| `OPENAI_API_KEY` | 之一 | OpenAI provider |
| `ANTHROPIC_API_KEY` | 之一 | Anthropic provider |
| `OLLAMA_HOST` | 否 | Ollama `base_url`(完整 URL 含 scheme) |
| `OLLAMA_API_KEY` | 否 | Ollama Cloud / 反代 Bearer |
| `REFLECT_PROVIDER` | 否 | 强制 provider(覆盖 TOML `[active]`) |
| `REFLECT_MODEL` | 否 | 覆盖默认模型规格;`=mock` 供 examples / e2e 离线运行 |
| `REFLECT_HOME` | 否 | 数据目录覆盖(默认 `~/.reflect`) |
| `REFLECT_AUTO_COMPACT_INPUT_TOKENS` | 否 | 压缩阈值(默认 10,000) |
| `REFLECT_MAX_ITERATIONS` | 否 | 图执行安全阀(最大迭代数) |
| `REFLECT_SANDBOX_*` | 否 | 沙箱策略(`STRICT` / `OS_LEVEL` / `WRITABLE`) |
| `RUST_LOG` | 否 | tracing filter(默认 `warn,reflect=info`) |

## 项目结构

```
crates/
├── protocol/               公共契约
│   └── reflect-protocol        Submission / Op / EventMsg + RolloutRecorder
├── abilities/              能力原语
│   ├── reflect-llm             ModelClient trait + OpenAI/Anthropic/Ollama + pricing
│   ├── reflect-tools           Tool trait + 23 内置工具 + ToolExecutionQueue
│   ├── reflect-hooks           HookEngine + 8 事件 + 7 决策
│   ├── reflect-skills          SKILL.md 扫描 + 加载
│   ├── reflect-memory          3 scope 记忆(Project/User/Session)
│   ├── reflect-agent-def       Markdown frontmatter Agent 定义
│   ├── reflect-prompt          分层 prompt + cache_control 注入
│   ├── reflect-compact         4 层压缩策略 escalation
│   ├── reflect-recovery        故障恢复
│   ├── reflect-notes           笔记
│   └── reflect-sanitize        输出净化
├── resources/              配置 / 权限 / 持久化
│   ├── reflect-config          统一 TOML 配置 + 热重载
│   ├── reflect-permissions     权限规则引擎
│   ├── reflect-plugin          插件 lifecycle
│   ├── reflect-rollout         JSONL 持久化 + session index
│   ├── reflect-telemetry       可观测性
│   ├── reflect-sandbox         多级沙箱
│   └── reflect-ast             tree-sitter 代码搜索
├── orchestration/          多 agent 编排
│   ├── reflect-subagent        Tool-per-Agent 子代理工厂(深度 ≤ 3)
│   ├── reflect-discussion      多 Agent 讨论编排
│   ├── reflect-task            结构化任务 / 团队
│   ├── reflect-pipeline        DAG 流水线
│   └── reflect-goal            目标模式
├── integrations/           外部协议集成
│   ├── reflect-mcp             MCP 客户端(stdio / streamable-http)
│   ├── reflect-lsp             LSP 集成
│   ├── reflect-integration     集成胶水层
│   └── reflect-stream          流式会话后端
└── runtime/                运行时与入口
    ├── reflect-core            AgentThread + 4 节点 StateGraph
    ├── reflect-exec            headless + JSONL stdout + resume
    ├── reflect-cli             CLI 顶层路由(单一 reflect 二进制)
    ├── reflect                 lib facade(60+ re-exports + Builder)
    └── reflect-py              PyO3 Python 绑定
```

`reflect` 是**唯一对外二进制**。TUI 二进制由独立仓库
[Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI) 提供。

## 文档

| 文档 | 内容 |
|------|------|
| [`AGENTS.md`](AGENTS.md) | 开发指南:命令、架构分层、项目约定 |
| [`SECURITY.md`](SECURITY.md) | 安全策略与漏洞上报 |

## 开发

```bash
make check                                # cargo check --workspace
make check-fast C=reflect-core            # 单 crate 快查
cargo test --workspace                    # 2200+ tests
make test-fast                            # cargo nextest(需已安装)
make test-changed                         # 只跑受变更影响的测试
cargo clippy --workspace --all-targets -- -D warnings   # strict lint
cargo fmt --all                           # 格式
./scripts/e2e.sh                          # 13 步端到端冒烟(构建+examples+headless+test+clippy)
./scripts/bench.sh                        # 性能基线(冷启动/首事件/RSS)
./scripts/discussion_smoke.sh             # discussion 编排冒烟
make dist                                 # 打包 DMG / tar.gz / zip 到 dist/
make gc                                   # 清理 target/ 中 cargo 不 GC 的中间产物
```

## 路线图

未来计划包括 MCP OAuth、PostgreSQL 会话存储、IDE 插件等。

## 致谢

- [开源 AI coding 工具参考](https://github.com/openai/codex) — 架构参考
- Python 原型实现 — 功能参考

## License

Apache-2.0
