# Plan Mode 设计

> 本文描述 Reflect 的 Plan mode(计划模式)协议与实现契约。
> 相关源码:`reflect-tools/src/builtins/enter_plan_mode.rs` /
> `exit_plan_mode.rs` / `plan_write.rs`,
> `reflect-hooks/src/builtins/plan_mode_gate.rs`,
> `reflect-protocol/src/event_msg/plan.rs`,
> `reflect-core/src/submission_loop.rs`(Op::PlanApproval 分发)。

## 1. 目的

任务复杂 / 影响面大时,agent 先**只读调研**并产出结构化计划,经用户
审批后再进入执行阶段。Plan mode 是 `PermissionMode::Plan` 的运行时
呈现:该模式下写类工具被 blanket-deny,agent 只能读、搜、写计划本体。

## 2. 进入路径(三条,殊途同归)

| 路径 | 触发方 | 行为 |
|---|---|---|
| 工具 `EnterPlanMode` | agent 自主判断 | 仅**发起请求**:发出 `EventMsg::PlanRequest`,不直接切模式;用户在 modal 按 1(批准)后 `submission_loop` 把权限模式切到 `Plan` |
| `reflect exec --plan-mode` | CLI flag | 启动即进入 Plan mode(headless 单 turn 场景) |
| TUI `/plan` slash | 用户 | 等价 exec flag,交互场景 |

防御性约束:已在 Plan 模式时,`PlanModeGate` deny `EnterPlanMode`
(重复调用只会造成无意义弹窗);`tool_exec` 节点另有兜底 —— 检测到
已处于 Plan 模式则跳过 PlanRequest 派发。

## 3. Plan 模式下的工具面

`PlanModeGate`(PreToolUse hook)采用**保守白名单**:

- 默认 allow:`read` / `grep` / `glob` / `echo` + Plan 控制面工具
  (`plan_write` / `exit_plan_mode` / `enter_plan_mode`);
- 其余一律 deny,`Deny.reason` 携带工具名(可观测,agent 能自行调整);
- 白名单可注入自定义(测试 / 高级场景,`PlanModeGate::with_allowlist`)。

## 4. 计划本体与审批协议

- `plan_write`:agent 写结构化计划(steps + status),产生
  `PlanStep` 事件流(TUI 渲染进度);
- `PlanCompletionHook` 跟踪完成度,计划完备后发 `PlanReady`;
- 用户决策走 `Op::PlanApproval { id, choice }`,与 `PlanRequestEvent.id`
  配对回送 waiter。选项:1 = 批准并退出 Plan mode 进入执行;
  2 = 拒绝(`PlanRejected`,agent 修订计划);3 = 中止;
- `ExitPlanMode` 工具是 agent 侧的收口:声明计划就绪,等待审批。

## 5. headless 语义(`reflect exec --plan-mode`)

单 turn 场景没有交互 modal:agent 直接以 Plan 模式跑完只读调研,
产出计划文本后 turn 结束。约定 **exit code 122 = 计划生成失败需重试**
(见 `ExecArgs::plan_mode` 的 doc-comment;供脚本化调用方判定)。

## 6. 设计原则(为什么这样切)

- **请求与切换分离**:`EnterPlanMode` 只发事件,切换由用户审批驱动 ——
  权限收紧必须由人确认,agent 不能自我提权(双向都成立:进入 Plan
  是收紧,退出 Plan 是放宽,都需要用户参与);
- **hook 而非硬编码**:工具面裁剪放在 `PlanModeGate` hook 层,权限
  模式本身保持纯数据 —— 其它模式(如未来的 AcceptEdits 变体)可复用
  同一机制;
- **协议配对**:`PlanRequest.id` ↔ `Op::PlanApproval.id` 的 waiter
  机制与 `ask_user_question` / 审批门同构,客户端实现零新增概念。
