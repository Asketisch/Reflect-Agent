# Reflect 协议(Protocol v0)

> 本文是协议层的总览与导航。**wire 协议(serve stdio JSONL)的完整
> 规范见 [`sdks/PROTOCOL.md`](../sdks/PROTOCOL.md)**;本文补齐 crate
> 内部引用需要的概览视角。协议 v0 已冻结:**新增** Op / EventMsg 变体
> 属 non-breaking,客户端必须容忍未知 `"type"` 字符串;改动既有变体
> 字段属 breaking,禁止。

## 1. 三个协议面

| 面 | 载体 | 定义位置 |
|---|---|---|
| Submission(Op) | 客户端 → AgentThread 的 mpsc 通道;`reflect serve` 上为 stdin 每行一个 JSON | `reflect-protocol/src/op.rs` |
| Event(EventMsg) | AgentThread → 客户端;serve 上为 stdout 每行一个 JSON | `reflect-protocol/src/event_msg/` |
| RolloutRecord | 持久化 JSONL(`~/.reflect/sessions/YYYY/MM/DD/<thread>.jsonl`) | `reflect-protocol/src/rollout.rs` |

三者共享 `ThreadId` / `TurnId` newtype 与 `ContentBlock` / `ToolError`
等基础类型(`item.rs`)。

## 2. Op(Submission)要点

- `UserInput`:一轮输入(items 携带 text / image 等);`Shutdown` /
  `Interrupt`:会话 / 在飞回合取消;`Steer`:回合中途转向
  (Now / Attachment 优先级);
- 审批复执:`ToolApproval` / `PlanApproval` / `AskUserQuestionResponse` /
  `AskUserInputResponse`(id 配对 waiter);
- 状态切换:`SetPermissionMode` / `SetEffort` / `EnterGoalMode` /
  `ExitGoalMode` / `Rewind`(对话回退,配合 `truncate_after`);
- serve 进程内控制:`RegisterTools` / `ToolExecutionResponse`(SDK
  宿主语言函数注册为远程工具)。

## 3. EventMsg 要点(按族)

- 生命周期:`SessionConfigured`(握手,id="" 为全局)/ `TurnStarted` /
  `TurnComplete` / `ShutdownComplete` / `submission_closed`;
- 模型流:`AgentMessage`(+delta)/ `AgentReasoning` / `TokenCount`;
- 工具:`ToolCallBegin` / `ToolOutput` / `ToolCallEnd`、后台任务通知;
- 审批 / 交互:`ApprovalRequest` / `PlanRequest` / `PlanReady` /
  `AskUserQuestion` / `AskUserInput`;
- 上下文:`ContextCompacted`(strategy: microcompact / smart_prune /
  llm_summarize / noop)/ `PermissionModeChanged`(全局事件,id="");
- 协作(Collab*):`CollabAgentSpawn*` 子代理生命周期、
  `SubagentStatus` 查询回执;**讨论编排不暴露独立协议状态** —— 状态
  经 transcript 持久化(`DiscussionTranscript` rollout 变体)呈现,
  客户端读会话记录即可还原;
- 任务板:`TaskEvent`(create / update / complete)。

## 4. RolloutRecord 要点

`SessionMeta`(首条)/ `Message` / `Compaction`(resume 回放压缩边界)/
`PermissionModeChanged`(from→to 轨迹)/ `Fork`(分叉标记)/
`DiscussionTranscript` / `Checkpoint` / `TokenCount`(token / 成本索引)。
文件 256 KiB 轮转 ×3,单条 16 KiB 截断 + 密钥脱敏(见
[docs/sanitize.md](sanitize.md));`truncate_after` 支持 rewind 物理截断
(先写 `.bak`)。

## 5. 版本策略

- 判别符 serde `snake_case`;未知变体容忍是**客户端义务**;
- 语义化承诺:Op / EventMsg 只增不改;如需字段演进,新增变体而非
  复用旧名;
- serve 的 SDK 扩展(如 `submission_closed`)在 PROTOCOL.md 单独
  标注引入版本。
