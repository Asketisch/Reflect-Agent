# 架构详解

> 本文承接 [README](../README.md)「架构总览」的深入版本:workspace 六层职责、
> 完整目录结构、核心数据流与关键设计决策。日常使用看 README 即可,
> 需要理解内部实现或二次开发时读本文。

## Workspace 分层(依赖方向自上而下)

33 个 crate 按 6 层分组于 `crates/<layer>/<crate>/`,依赖只能自上而下
(上层依赖下层,protocol 是所有层的公共契约):

| 层 | Crate | 职责 |
|----|-------|------|
| `protocol/` | reflect-protocol | Submission / Op / EventMsg / Item + RolloutRecorder trait。所有层的公共契约 |
| `abilities/` | llm, tools, hooks, skills, memory, agent-def, prompt, compact, recovery, notes, sanitize | 能力原语:ModelClient trait + providers、Tool trait + 内置工具、HookEngine、压缩 escalation 等 |
| `resources/` | config, permissions, plugin, rollout, telemetry, sandbox, ast | 配置热重载、权限规则、插件 lifecycle、JSONL 持久化、tree-sitter 代码搜索 |
| `orchestration/` | subagent, discussion, task, pipeline, goal | 多 agent 编排:子代理工厂(并发在途≤16)、讨论编排、任务/团队、DAG 流水线、目标模式 |
| `integrations/` | mcp, lsp, integration, stream | MCP 客户端(stdio/streamable-http)、LSP、流式会话后端 |
| `runtime/` | core, exec, cli, reflect, py | AgentThread + StateGraph、headless、CLI 入口、lib facade(60+ re-exports + Builder)、PyO3 绑定 |

## 完整目录结构

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
│   ├── reflect-subagent        Tool-per-Agent 子代理工厂(并发在途 ≤ 16)
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
    ├── reflect-exec            headless + JSONL stdout + resume + serve
    ├── reflect-cli             CLI 顶层路由(单一 reflect 二进制)
    ├── reflect                 lib facade(60+ re-exports + Builder)
    └── reflect-py              PyO3 Python 绑定(同步 run API)
sdks/                          多语言 SDK
├── PROTOCOL.md                    serve wire 协议规范
├── python/                        纯 stdlib Python SDK(reflect-agent)
└── typescript/                    纯 TS npm SDK(reflect-agent)
```

`reflect` 二进制由 `reflect-cli` crate 产出,是本仓库的**薄 headless 入口**;
框架消费方的主接口是 `reflect` 库门面(Builder / re-exports)。

## 核心数据流

- `AgentThread`(reflect-core)消费 `Submission`(mpsc 通道),`submission_loop` 驱动
  4 节点 `StateGraph`:**PreLoop → ModelCall → (ToolExec → PreLoop)\* → CheckStop**。
  模型不再发工具调用时经 CheckStop 结束 turn(Stop hook 可否决强制续跑)。
- `NodeContext` 携带回合级依赖:model registry、RoutingPolicy、HookEngine、
  ToolExecutionQueue、event 通道、CancellationToken、approval gates。
- 客户端(TUI / exec / serve / lib)统一通过 Submission/Event 协议通信;headless
  模式 Event 以 JSONL 流写 stdout、tracing 日志走 stderr(`reflect exec | jq`
  不被破坏);`reflect serve` 复用同一协议常驻服务,SDK 在其上封装
  (远程工具请求 / 回执、审批等交互均为协议内 Op / EventMsg)。

各节点的职责划分:

| 节点 | 职责 |
|------|------|
| PreLoop | 压缩 escalation 判定(按输入 token 阈值逐级升级)、hooks 前置处理、history 组装 |
| ModelCall | 调用 ModelClient(SSE 流式),产出文本增量与工具调用 |
| ToolExec | 审批 gate 检查 → 经 ToolExecutionQueue 执行工具(内置 / MCP / 远程)→ 结果回填 history |
| CheckStop | 模型无新工具调用时判定回合结束;Stop hook 可否决并强制续跑 |

## 工具面裁剪

发给 LLM 的工具 schema 由 `ToolRegistry::list_specs()` 产出后再经 PreLoop
过滤 —— 未注册或被裁剪的工具**根本不进 prompt**。裁剪有三层入口,
按"面向谁"划分:

### 1. Agent 定义层(Markdown frontmatter,主入口)

Agent 定义是带 frontmatter 的 Markdown 文件,放在
`.reflect/agents/`(workspace)或 `~/.reflect/agents/`(home;同名时
home 定义覆盖 workspace —— 注意与 subagents 的优先级方向相反),
`reflect exec --agent <name>` 激活:

```markdown
---
name: reviewer
description: 只读代码审查
readonly: true                  # 排除变更类工具(READONLY_DENYLIST)
tools: [read, grep, glob]       # 白名单:交集
disallowed_tools: [bash]        # 黑名单:差集
---
You are a read-only code reviewer...
```

过滤在 PreLoop 每 turn 生效,语义为 **白名单 ∩、黑名单 −、`readonly`
排除变更类**;三个字段全空则不过滤(向后兼容)。也可以在代码里直接构造
`AgentDefinition` 并经 `AgentConfig::with_m4` 注入(字段全部公开)。
另:Plan mode 下 `write` / `edit` 会被强制移除,LLM 只能用路径受限的
`PlanWrite`。

### 2. 子代理层(config.toml / Markdown)

`[[subagents]]` 的 `allowed_tools` 白名单决定子代理的工具面:

```toml
[[subagents]]
name = "Explorer"
role = "explorer"
system_prompt = "Inspect the codebase and return a concise summary."
allowed_tools = ["bash", "read", "grep", "glob"]
```

来源合并优先级(后者覆盖同名):`~/.reflect/subagents/*.md` →
workspace `.reflect/subagents/*.md` → config.toml `[[subagents]]`。
spawn 时为子代理构造**仅含白名单工具**的过滤后 registry(coordinator
模式下另有 worker 专用裁剪:用 `register_except` 排除 `TeamCreate` 等
内部工具)。

### 3. 代码层(ToolRegistry API)

库用户经 `agent.thread().tools()` 拿到 registry 直接增删:

| API | 用途 |
|-----|------|
| `register` / `register_with_source` | 注册单个工具(自定义 `Tool` trait 实现、MCP / 插件 / 运行时工具各有专用入口) |
| `unregister(name)` | 单摘一个工具 |
| `unregister_source(source)` | 按来源整组移除(如一键摘掉全部 MCP 工具) |
| `register_except(tools, excluded)` | 批量装配 + 排除名单 |

`custom_tool` 示例(`cargo run -p reflect --example custom_tool`)演示
了注册自定义工具的标准写法。



- **Protocol v0 已冻结**:新增 Op/EventMsg 变体 = non-breaking,不可改动既有变体。
  wire 协议的稳定性是所有客户端(TUI / exec / serve / SDK)共同的前提。
- **JSONL stdout,日志 stderr**:`reflect exec | jq` 不破坏 —— 事件流与诊断日志
  严格分流,管道消费方永远拿到纯 JSONL。
- **ToolError 定义在 reflect-protocol**:打破 tools ↔ hooks 循环依赖,错误类型
  下沉到公共契约层。
- **StateGraph 用 enum + match**:4 节点固定结构,手写转移,不引入 petgraph ——
  控制流显式、可审计,这正是框架相对黑盒 agent 循环的核心差异之一。
- **错误处理**:库 crate 用 `thiserror`(错误类型显式),应用层(exec/cli)
  用 `anyhow`(错误链传播)。
- **serve 是嵌入入口,不是开发助手**:内置 hook 默认不启用(exec / TUI 保持
  "未配置 = 全启用"),避免 verification 之类 Stop hook 在宿主 cwd 跑测试 ——
  serve 的宿主是 SDK 使用者的进程,而非开发者本人。
