## 项目概览

烛夜（ZhuYe）— Rust 编写的 AI agent 运行时**框架**:4 节点 StateGraph 引擎、内置工具 + hooks、
多 LLM provider、MCP/LSP 集成、JSONL rollout 持久化。本仓库是**纯框架层**:
主接口为 `zhuye` 库门面(Builder + 60+ re-exports)与 Rust / Python 集成入口;
`zhuye-cli` 只是附带产出的薄 headless CLI 入口(单一 `zhuye` 二进制,
**不含** `tui` 子命令)。

- Rust stable,edition 2024,MSRV 1.85(rust-toolchain.toml 锁定)
- 33 crate workspace,按 6 层分组于 `crates/<layer>/<crate>/`

## 常用命令

```bash
# 构建(dev / release)
cargo build -p zhuye-cli                # 产出 target/debug/zhuye
make build                                # release 等价:cargo build --release -p zhuye-cli

# 类型检查
cargo check --workspace                   # 或 make check
make check-fast C=zhuye-core            # 单 crate 快查

# 测试
cargo test --workspace                    # 全量(CI 用 --all-targets)
make test-fast                            # cargo nextest(需已安装 cargo-nextest)
make test-changed                         # nextest 只跑自 HEAD 起受变更影响的测试
cargo test -p zhuye-core <test_name>    # 单个测试
cargo test -p zhuye-tools -p zhuye-hooks    # 多 crate
./tests/run_all.sh functional             # 功能覆盖门禁(能力覆盖层 + Rust 参数面 + 双 SDK 绑定层)
./tests/run_all.sh functional-layer       # 只跑能力覆盖层(Python 打真二进制,原始 wire)

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

# SDK 发布(PyPI / npm,详见 docs/release.md)
./scripts/release_sdk.sh check            # 版本一致性 + 双包构建检查(dry-run,不发布)
```

离线开发:examples 与 e2e 用 `ZHUYE_MODEL=mock` 跳过真实 LLM 网络调用。

## 架构

### Workspace 分层(依赖方向自上而下)

| 层 | Crate | 职责 |
|----|-------|------|
| `protocol/` | zhuye-protocol | Submission / Op / EventMsg / Item + RolloutRecorder trait。所有层的公共契约 |
| `abilities/` | llm, tools, hooks, skills, memory, agent-def, prompt, compact, recovery, notes, sanitize | 能力原语:ModelClient trait + providers、Tool trait + 内置工具、HookEngine、压缩 escalation 等 |
| `resources/` | config, permissions, plugin, rollout, telemetry, sandbox, ast | 配置热重载、权限规则、插件 lifecycle、JSONL 持久化、tree-sitter 代码搜索 |
| `orchestration/` | subagent, discussion, task, pipeline, goal | 多 agent 编排:子代理工厂(并发在途≤16)、讨论编排、任务/团队、DAG 流水线、目标模式 |
| `integrations/` | mcp, lsp, integration, stream | MCP 客户端(stdio/streamable-http)、LSP、流式会话后端 |
| `runtime/` | core, exec, cli, zhuye, py | AgentThread + StateGraph、headless、CLI 入口、lib facade(60+ re-exports + Builder)、PyO3 绑定 |

### 核心数据流

- `AgentThread`(zhuye-core)消费 `Submission`(mpsc 通道),`submission_loop` 驱动
  4 节点 `StateGraph`:**PreLoop → ModelCall → (ToolExec → PreLoop)\* → CheckStop**。
  模型不再发工具调用时经 CheckStop 结束 turn(Stop hook 可否决强制续跑)。
- `NodeContext` 携带回合级依赖:model registry、RoutingPolicy、HookEngine、
  ToolExecutionQueue、event 通道、CancellationToken、approval gates。
- 客户端(TUI / exec / lib)统一通过 Submission/Event 协议通信;headless 模式
  Event 以 JSONL 流写 stdout、tracing 日志走 stderr(`zhuye exec | jq` 不被破坏)。
- 关键设计决策:
  - **Protocol v0 已冻结**:新增 Op/EventMsg 变体 = non-breaking,不可改动既有变体
  - `ToolError` 定义在 zhuye-protocol,用于打破 tools ↔ hooks 循环依赖
  - StateGraph 用 enum + match 手写转移,不引入 petgraph
  - 错误处理:库 crate 用 `thiserror`,应用层(exec/cli)用 `anyhow`

## 开发流程检查表(改任何代码前必读)

**新功能**或 **bug 修复**提交前,必须逐条走完:

1. **能力覆盖脚本同步** —— 判断本次改动是否需要新增 / 更新能力覆盖用例:
   - 改了 agent 能力(serve 协议 Op / EventMsg / 内置工具 / 审批 / 编排 /
     持久化 / 重连恢复 / CLI 能力面)
     → 必须在 `tests/functional/` 有对应用例,断言到**字段级**
       (不接受"退出码 0"或"不抛错"作为能力断言);
   - 已有相关用例 → 检查是否需要**更新**(断言口径变了 / 新增字段 / 语义变化);
   - 能力覆盖用例**只写在 `tests/functional/`** —— 不要在 SDK 测试里重复写
     协议/能力断言;`tests/rust/*.sh` 是 CLI **参数面**的老套件,新增能力
     不往里加(能力面统一用 Python 写)。
2. **SDK 测试脚本同步** —— 改了协议 / SDK 公开行为 / 事件形状时,**两个 SDK
   都要看**:
   - Python SDK:`sdks/python/tests/`(纯逻辑,mock serve)与
     `tests/sdk-python/`(真二进制绑定层);
   - TS SDK:`sdks/typescript/tests/`(纯逻辑,mock serve)与
     `tests/sdk-ts/`(真二进制绑定层);
   - 协议变更是 additive 也必须同步 —— SDK 遇到未知变体要能降级不崩。
3. **覆盖矩阵同步** —— `tests/COVERAGE.md` 的「能力 × 覆盖层」映射表随改动
   更新:新增能力补一行;移除能力移到「未覆盖项」并写明原因;发现的 SDK 缺陷
   记入「已知缺陷」。
4. **验证** —— 按下方「验证矩阵」跑对应命令,全绿再提交。
5. **注释与提交信息** —— 注释一律中文;commit 用 `type(scope): 描述`
   conventional 风格。

## 验证矩阵(按改动范围选跑)

| 改动范围 | 必跑命令 |
|---|---|
| 任何改动 | `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` |
| Rust 核心 / crate | `cargo test --workspace` |
| serve 协议 / 事件 / 持久化 / 工具 / 编排 / 重连 | `./tests/run_all.sh functional`(能力覆盖层 + Rust 参数面 + 双 SDK 绑定层) |
| 仅 SDK 绑定逻辑 | `./tests/run_all.sh sdk-python` + `./tests/run_all.sh sdk-ts` |
| CLI 子命令参数面 | `./tests/run_all.sh rust` |
| 文档 / 注释 | 无需跑测试,自查链接与命令是否仍有效 |

## 约定

- **强制:注释一律使用中文** —— 新增或修改代码时,行注释(`//`)、doc-comment
  (`///`、`//!`)、测试说明等必须用中文书写,不得使用英文注释;commit message
  亦用中文,遵循 `type(scope): 描述` conventional 风格
- 约一半 lib.rs 顶部有 `#![allow(clippy::...)]` 宽松头(clippy.toml 中
  `avoid-breaking-exported-api = false`);新代码仍以 `-D warnings` 全绿为门槛
- `[profile.dev]` 刻意关闭 incremental、`[profile.release-fast]` 供本地快速迭代
  (见根 Cargo.toml 注释;发布分发用默认 release,thin LTO)
