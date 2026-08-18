## 项目概览

Reflect — Rust 编写的 AI agent 运行时:4 节点 StateGraph 引擎、内置工具 + hooks、
多 LLM provider、MCP/LSP 集成、JSONL rollout 持久化,产出单一 `reflect` CLI 二进制。

- Rust stable,edition 2024,MSRV 1.85(rust-toolchain.toml 锁定)
- 33 crate workspace,按 6 层分组于 `crates/<layer>/<crate>/`
- 注:TUI 已拆分至独立仓库 Reflect-TUI,本仓库的 `reflect` 二进制**不含** `tui` 子命令
  (README 中 `reflect tui` 相关段落已过时)

## 常用命令

```bash
# 构建(dev / release)
cargo build -p reflect-cli                # 产出 target/debug/reflect
make build                                # release 等价:cargo build --release -p reflect-cli

# 类型检查
cargo check --workspace                   # 或 make check
make check-fast C=reflect-core            # 单 crate 快查

# 测试
cargo test --workspace                    # 全量(CI 用 --all-targets)
make test-fast                            # cargo nextest(需已安装 cargo-nextest)
make test-changed                         # nextest 只跑自 HEAD 起受变更影响的测试
cargo test -p reflect-core <test_name>    # 单个测试
cargo test -p reflect-tools -p reflect-hooks    # 多 crate

# Lint / 格式(CI 强制,-D warnings)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# 文档
cargo doc --no-deps

# 端到端 / 基准
./scripts/e2e.sh                          # 13 步冒烟:构建+examples+headless+test+clippy
./scripts/bench.sh                        # 性能基线(冷启动/首事件/RSS)
./scripts/discussion_smoke.sh             # discussion 编排冒烟

# 打包分发 / 磁盘治理
make dist                                 # DMG / tar.gz / zip 到 dist/
make gc                                   # 清 target/debug 中 cargo 不 GC 的中间产物(曾膨胀 30+GB)
```

离线开发:examples 与 e2e 用 `REFLECT_MODEL=mock` 跳过真实 LLM 网络调用。

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
- 关键设计决策:
  - **Protocol v0 已冻结**:新增 Op/EventMsg 变体 = non-breaking,不可改动既有变体
  - `ToolError` 定义在 reflect-protocol,用于打破 tools ↔ hooks 循环依赖
  - StateGraph 用 enum + match 手写转移,不引入 petgraph
  - 错误处理:库 crate 用 `thiserror`,应用层(exec/cli)用 `anyhow`

## 约定

- **强制:注释一律使用中文** —— 新增或修改代码时,行注释(`//`)、doc-comment
  (`///`、`//!`)、测试说明等必须用中文书写,不得使用英文注释;commit message
  亦用中文,遵循 `type(scope): 描述` conventional 风格
- 约一半 lib.rs 顶部有 `#![allow(clippy::...)]` 宽松头(clippy.toml 中
  `avoid-breaking-exported-api = false`);新代码仍以 `-D warnings` 全绿为门槛
- `[profile.dev]` 刻意关闭 incremental、`[profile.release-fast]` 供本地快速迭代
  (见根 Cargo.toml 注释;发布分发用默认 release,thin LTO)
