# 功能覆盖矩阵(测试 ↔ 功能面映射)

本矩阵回答一个问题:**当前测试是否完整覆盖了全部对外功能**(而非冒烟)。
覆盖口径:每条功能面至少有一个**字段级/行为级断言**(非"退出码 0")的
用例;断言失败即失败。全程离线(`REFLECT_MODEL=mock`)+ 隔离 HOME。

## 0. 覆盖分层:谁测什么

| 层 | 位置 | 测谁 | 依赖 SDK? | 规模 |
|---|---|---|---|---|
| **能力覆盖层**(主) | `tests/functional/` | **Rust 代码在真产物上的行为**(原始 JSONL wire) | ❌ | 51 条 |
| CLI 参数面 | `tests/rust/*.sh` | 17 个子命令的参数矩阵与输出契约 | ❌ | 320+ 断言 |
| SDK 绑定层 | `tests/sdk-python/`、`tests/sdk-ts/` | 两个**客户端自己**的行为 | ✅ | Python 9 条 / TS 5 条 |
| SDK 单测 | `sdks/*/tests/` | SDK 纯逻辑(mock serve),随包发布 | ✅ | Python 7 / TS 5 条 |
| crate 单测 | `crates/**/tests/` | 内部单元与集成 | — | 既有 |
| **实模型演练层**(可选) | `tests/live/` | **真实模型**下的引擎行为(协议形状) | ❌ | 12 条 |

**新增能力断言只落能力覆盖层一处** —— 不要在 SDK 测试里重复写协议/能力断言。
理由:SDK 自身的 bug 会把 Rust 的 bug 遮住(见 §6 已知缺陷 D1)。

⚠️ **实模型演练层不算能力覆盖** —— 它断言的是**协议形状与运行时行为**
(事件序 / 终态 / usage 字段 / 工具成对),**不断言具体回答文本**。
能力断言仍然只在 `tests/functional/`。两层不重叠、不互相替代:
能力覆盖层是 CI 强制门禁(离线、确定性),实模型层是可选手动/夜间门禁
(需外网与免费额度,默认 SKIP)。

## 1. 能力 × 覆盖层矩阵

| 能力域 | 能力覆盖层 | CLI 参数面 | SDK 绑定层 | crate 单测 | 实模型层 |
|---|---|---|---|---|---|
| 基础通信(握手/单轮/多轮/事件流/终态) | D1–D5 | cli_core | K1–K2 | ✅ | L1–L4 |
| 并发与路由 | S1–S3 | — | — | — | — |
| 内置工具调用 | T1–T2 | tools_drive | — | ✅ | T1–T3 |
| 远程工具(注册/往返/失败/超时) | T3–T4、X4 | serve_ops | K3、K8 | ✅ | — |
| 审批交互(approve / deny / bubble) | T5–T6 | integrations | — | ✅ | — |
| 会话状态 Op(19 Op) | A1–A12 | serve_ops | K4 | ✅ | — |
| 重连与恢复(resume / -c / -r / 崩溃 / EOF) | R1–R5 | cli_core(exec --resume) | — | ✅ | P2 |
| 容错与失败恢复 | X1–X6 | — | — | ✅ | X1 |
| 持久化(rollout / 截断 / 索引) | P1–P4 | cli_deep(session) | — | ✅ | P1–P2 |
| 编排(子代理/讨论/任务/团队/流水线/goal) | O1–O5 | cli_modules、cli_deep | — | ✅ | — |
| CLI 能力面 | C1–C4 | cli_deep(194 断言) | — | — | — |
| 模型发现(清单接口 / 免费判定 / 动态 model) | — | models_* | — | ✅ | — |

## 2. 能力覆盖层明细(`tests/functional/`)

### D 组 基础通信与事件流(`test_serve.py`)
- **D1 握手**:`session_configured` 是全局事件(`id=""`),`model=mock/mock-1`、
  `provider=mock`、含 `session_id` / `approval_policy` / `sandbox_policy`。
- **D2 单轮事件序列**:`turn_started` → `agent_message_delta` **×2**(mock 把
  文本拆两段)→ `token_count` → `turn_complete`;`status=="success"`(wire 真值,
  不是类型注释里的 `ok`);`usage` 四字段齐全;`turn_id` 首尾一致。
- **D3 多轮上下文**:两轮拿到 mock 脚本第 1、2 行。
- **D4 token_count**:含 `input_tokens`/`output_tokens`,且至少一条带
  `model` / `provider` 维度(v1.x stats 字段)。
- **D5 终态语义**:turn 类以 `turn_complete` 收尾(其后可能跟一条
  `submission_closed`);非 turn 类只有 `submission_closed`。

### S 组 并发与路由(`test_serve.py`)
- **S1 隔离**:并发提交两条 `user_input` → 两条流的事件 id 各自归属、
  互不串台;各成一个独立 turn(turn_id 不同);两条 mock 回复各用一次。
- **S2 独立收尾**:并发两个 turn 各自恰好一次 `turn_started` /
  `turn_complete`,状态 `success`。
- **S3 收尾不变量**:每条 submission 恰好一条 `submission_closed`。

### T 组 工具与审批(`test_tools.py`)
- **T1** 内置 `echo` → `tool_call_begin` → `tool_call_end`(回显文本、
  `is_error=false`)。
- **T2** 模型调未注册工具 → `tool_call_end(is_error=true, 含 tool not found)`,
  turn 不死。
- **T3** 远程工具往返:`register_tools` → 模型调用 → `tool_execution_request`
  (全局,带 `tool`/`args`)→ 回执 → `tool_call_end`(输出透传)。
- **T4** 远程工具失败回执 → `tool_call_end(is_error=true)`。
- **T5** 工具审批全链路(`REFLECT_APPROVALS=1` + MCP 工具):`approval_request`
  (request_id / kind.tool_name / risk)→ 回 `tool_approval` → 工具真执行 →
  `turn_complete`。
- **T6** 审批 deny → 工具**未执行**、`tool_call_end(is_error=true, 含 deny 理由)`、
  turn 仍收尾。

### A 组 会话状态 Op(`test_serve.py`)
`user_input`+`thread_settings`(四字段) / `set_effort`(静默) / 权限模式切换
(全局 `permission_mode_changed` + 持久化轨迹) / `rewind`(`turn_rewound`) /
`interrupt`(`turn_aborted`,reason 是带 `type` 的对象) / `compact`
(`context_compacted`) / goal 模式(进出静默) / plan 模式(`plan_request` →
`plan_ready` → 切权限模式)/ 四种孤儿回执(不崩、收尾、会话存活) /
`steer`(静默,消息合并进下一轮 user message)/ `query_subagents`
(`subagent_status`)/ `prompt_command`(`prompt_list` / `prompt_switched`)。

### R 组 重连与恢复(`test_recovery.py`)
- **R1 `--resume <thread id>`**:恢复会话并**续写同一份** rollout(历史仍在)。
- **R2 `-c`** / **R3 `-r 1`**:续最近一次会话。
- **R4 进程崩溃**:挂起的 submission **收不到终态事件** —— 客户端靠 stdout
  EOF 感知断开(wire 上没有"会话结束"事件,SDK 层负责兜底)。
- **R5 stdin EOF**:进程退出码 0(EOF 等价于 shutdown)。

### X 组 容错(`test_recovery.py`)
坏行 → `invalid_submission` 且会话存活 / 空工具名 → `tool_name_invalid` /
撞内置名 → `tool_name_conflict` / 远程工具超时 → 错误输出含 `timed out`
且 turn 收尾 / 未知 op → `invalid_submission` 且会话存活 / 错误事件是全局
(`id=""`),不进 turn 流。

### P 组 持久化(`test_persistence.py`)
`session_meta` 首行 + `message` 记录集 / 权限模式 `from→to+at` 轨迹 /
rewind 物理截断(记录减少 + `.rewind-<ts>.bak` 备份)/ `reflect session ls`
能列出该会话。

### O 组 编排(`test_orchestration.py`)
子代理 `call_explorer`(子代理执行 → 回执进历史)/ 讨论编排(transcript +
Result + rounds_completed)/ 任务与团队(create → ls)/ 流水线错误路径
(no runner / team not found)/ goal 模式与其他 op 共存。

### C 组 CLI 能力面(`test_cli.py`)
`serve` 三个恢复入口 / `exec` 输出每行是合法 JSON 且含 turn 事件 /
`session ls` + `show` 报错路径 / `doctor` + `version` 输出契约。

## 3. CLI 子命令(17 个,不含 help)

| 子命令 | 冒烟(cli_core/cli_modules) | 深度断言位置 |
|---|---|---|
| `exec` | ✅ | cli_deep(exec JSONL 字段级)、tools_drive(全工具驱动)、能力层 C2 |
| `serve` | ✅ | **能力层 19 Op 全量** + serve_ops.sh(FIFO 驱动,逐 op 断言 EventMsg)+ integrations(MCP/bubble/热重载) |
| `login` | ✅ | cli_deep:4 provider 路径、缺 key 报错、短 key 告警、`--force`、api_key 脱敏 |
| `mcp` | ✅ | cli_deep:stdio+http 双传输 add/互斥报错/ls/show/remove/`test`(真 mock server 成功+坏 binary 失败)/registry 三路径 |
| `config` | ✅ | cli_deep:set/unset/ls/show/`--reveal`/非法 key 报错 |
| `session` | ✅ | cli_deep:双 ID 前缀解析回归(文件名前缀+内部 id 前缀+歧义报错)/rename/export/fork/rm;能力层 P4/C3 |
| `traces` | ✅ | cli_deep:ls/show 前缀/不存在报错 |
| `doctor` | ✅ | cli_deep:常规 + `--check-network`(mock provider DNS 失败仍 rc=0);能力层 C4 |
| `plugin` | ✅ | cli_deep:marketplace add/ls/refresh/remove + 安装 + info/show/enable/disable/uninstall 全链路 |
| `lsp` | ✅ | cli_deep:空配置提示/未配置 status 报错/配置后 list+status |
| `task` | ✅ | cli_deep:create/ls/show/update/stop/purge + team 全链路 + 非数字 id clap 报错;能力层 O3 |
| `pipeline` | ✅ | cli_deep 错误路径(team 不存在/自定义 label 无 runner —— 产品限制,见 §5);能力层 O4 |
| `discussion` | ✅ | cli_deep:mock 驱动真多 agent(sequential,transcript 非空)+ `--output`;能力层 O2 |
| `security` | ✅ | cli_modules(cargo audit 封装,断言输出结构) |
| `workspace` | ✅ | cli_deep:ls/clone 本地仓库/`--depth` 浅克隆/坏 URL 报错 |
| `update` | ✅ | cli_deep:常规 + `--check-only` |
| `version` | ✅ | cli_core(断言版本号格式);能力层 C4 |

## 4. 集成面

`integrations.sh`(44 断言):
- **MCP**:server 生命周期(`mcp_server_started` 含 tool_names/tool_count、
  坏 server → `mcp_server_failed`)+ in-turn 调用(`mcp_tool_invoked` +
  `tool_call_end` 回传 echo 输出);能力层 T5/T6 另覆盖审批链路。
- **skill**:工作区 `SKILL.md` 扫描 + `load_skill` 激活(正文 + 激活工具
  列表);不存在的 skill 优雅报错。
- **notes**:`add_session_note` → `~/.reflect/session-notes/<thread>.jsonl`
  落盘(内容 + JSON 合法性)。
- **bubble 审批**:Bubble 模式下 Prompt 权限工具 → `permission_bubble`
  非阻塞通知 + 自动放行执行。
- **配置热重载**:`config_reloaded`(path + sections_changed)+ 重载后会话存活。
- **plugin**:marketplace add/install/enable → 启动期 `plugin_loaded`
  (plugin 名/version/skill_count)。

## 5. 已知产品限制(测试断言当前行为,防退化)

- ~~`pipeline run`:plan 预设模板硬编码 `{{input.audience}}`~~(v1.6 已修复:
  planner 预设只依赖 `{{topic}}`,CLI `--input k=v` 注入模板 inputs,
  build_factory 从配置引导 registry 使 mock 可离线命中);
  `pipeline team` 失败节点存在时退出非 0(v1.6 对齐 CI 语义)。
- `discussion run`:concurrent 模式受 SubAgentFactory 深度上限约束;
  无 provider 时降级 `run_noop`(退出 0,Result JSON 仍含 outcome)。
- `approval_request` / `ask_user*` 的**交互式工具路径**需要 TUI/modal;
  headless 下只验孤儿回执的 wire 级健壮性。serve + `REFLECT_APPROVALS=1`
  可触发真实 gate(能力层 T5/T6 已覆盖)。
- `web_search` / `web_fetch` 离线时优雅报错(无 provider key / 无网络)。
- **并发 turn 是真并发**:core 对每个 `UserInput` 提交 `tokio::spawn` 一个
  独立 task(`submission_loop.rs:621`),两个 turn 的模型调用先后不保证 ——
  与 `sdks/PROTOCOL.md` §4「core 侧串行排队」的措辞不符(见 §6 D5)。
- **三个 id 互不相同**:`session_configured.session_id`、rollout 内
  `session_meta.session_id`、会话**文件名**里的 thread id 是三个不同的
  uuid(exec/serve 路径未注入 `with_session_id`)。`--resume` 的参数必须用
  **文件名**里的 id;`reflect session ls` 显示的是 SessionMeta 里的 id。

## 6. 已知缺陷(本次只记录不修,供后续独立任务)

| # | 缺陷 | 证据 | 影响 |
|---|---|---|---|
| D1 | **Python SDK `close()` 从不发出 shutdown op**:先置 `_closed=True` 再调 `_write_submission`,后者因 `_closed` 抛 `RuntimeError("agent is closed")` 被 `except Exception: pass` 吞掉 | `sdks/python/reflect/agent.py:259-266` + `:373-382` | 文档承诺的"发 Shutdown → 等 ShutdownComplete"未兑现;实际靠关 stdin(EOF)兜底。TS 侧用 `writeSubmissionUnchecked` 正常发出 —— **两客户端行为相反** |
| D2 | 协议类型与 wire 不符:`TurnComplete.status` 类型写 `ok/cancelled/aborted/error`,wire 实为 `success/max_iterations/stopped/token_budget_exceeded`;`TurnAborted.reason` 类型写 `str`,wire 实为 `{"type":"user_interrupt"}`;`PlanApprovalChoice` 类型写 `auto_mode/prompt_mode/reject`,wire 实为 `auto_mode/manual_approve/revise` | `sdks/python/reflect/protocol.py:228-245` vs `event_msg/turn.rs:75-92`、`item.rs:249-260`;TS 侧前两处同样错 | 按类型注解写代码即错 |
| D3 | Python `ReviewDecision` TypedDict 建模为 `{"type":"approve"}`,wire 实为裸字符串 `"approve"` / `"approve_for_session"` / `{"deny":{"reason":...}}` | `sdks/python/reflect/protocol.py:62-71` vs `item.rs:236-241` | 按类型注解传 `{"type":"approve"}` → 服务端反序列化失败 → `invalid_submission` → **审批永久挂起**(TS 侧 v1.3 已修) |
| D4 | 全局事件(`id=""`)无公开读取口:`permission_mode_changed` / `config_reloaded` / `mcp_server_started` / `error` 只能读私有 `_global_queue` | `sdks/python/reflect/agent.py:322-345` | SDK 用户拿不到全局事件;本次能力层用原始 wire 覆盖,SDK 侧仍缺 |
| D5 | 协议文档与实现不符:`sdks/PROTOCOL.md` §4 称「core 侧串行排队」,实测每个 `UserInput` 提交一个独立 task(真并发) | `submission_loop.rs:621`;能力层 S1/S2 实测 | 依赖"第二个 turn 一定在第一个之后开始"的客户端会踩坑;文档应改为"并发执行、事件按 id 路由" |
| D6 | `tests/` 套件此前从未进 CI,导致**已存在的失败用例**长期未被发现(原 `test_sdk_real_bin.py` 的 interrupt 断言与当前 core 行为不符;`cli_core.sh` 的 `exec --plan-mode` 期望 0 而实际按约定 122;`serve_ops.sh` 期望「不 emit plan_approved」而引擎现在会发;`tools_drive.sh` 的 grep 期望 `./` 前缀而实际无前缀) | 本批次修正前实测失败 | 本批次已修正断言(不改服务端行为),并由 CI job `functional-suite` 收口 |
| D7 | **bash 工具对被沙箱拒绝的命令仍报成功**:macOS Seatbelt 下越界写被拦下(stderr 含 `Operation not permitted`、文件不落地),但工具输出 `is_error=false`(退出码经 sentinel 回报为 0) | 本地实测 `reflect exec` + bash 越界写;`tests/rust/tools_drive.sh` 的沙箱用例 | 模型看不到失败信号,可能误以为写成功;安全不变量(文件不落地)成立,但错误信号缺失 |
| D8 | **`InvalidRequest`(400)被归到 `CooldownReason::Auth`**(默认 1 小时冷却) | `crates/runtime/reflect-core/src/graph/nodes/model_call/stream.rs` 的 `build_cooldown_reason` 兜底分支 | 一次 400(如模型不支持 tools)会让该凭据停摆一小时,远超合理范围;本次未改(超出最小改动边界),记此备查 |

### 实模型层观测到的上游限制(非 Reflect 缺陷)

以下来自 `tests/live/` 对真实免费池的观测,**不计入缺陷**,但影响实模型层的
跑法与断言口径:

| 现象 | 说明 | 应对 |
|---|---|---|
| 429 限流频繁 | 免费池配额是**全局**的;OpenCode 车道按会话计额度,`retry-after` 可达 12h | live 层把 429 判为**跳过**而非失败;矩阵**串行**跑 |
| 部分模型不支持 tools | provider 直接 400(如 `dots-studio/dots-3-note-preview:free`) | 能力探测识别并跳过,不写死黑名单 |
| 首字延迟大 / 思考静默 | 实测首字 >30s、思考阶段可静默 60~70s | `timeout_secs` 放大到 300(默认 60s 是 reqwest 的**总**请求时长,会腰斩流式响应) |
| 模型集合随时变 | 上游随时下架某个 id | 清单每次实时拉取;`[routing].fallbacks` 跨模型降级兜底 |
| prompt 可能被上游记录 | 免费池模型卡明示 | 实模型层只用合成数据 |

## 7. 用例规模汇总

| 文件 | 规模 |
|---|---|
| `functional/test_serve.py` | 21 条(D5 + S3 + A13) |
| `functional/test_tools.py` | 6 条(T6) |
| `functional/test_recovery.py` | 11 条(R5 + X6) |
| `functional/test_persistence.py` | 4 条(P4) |
| `functional/test_orchestration.py` | 5 条(O5) |
| `functional/test_cli.py` | 4 条(C4) |
| **能力覆盖层合计** | **51 条** |
| `live/test_live_basic.py` | 4 条(L1–L4) |
| `live/test_live_tools.py` | 3 条(T1–T3) |
| `live/test_live_persistence.py` | 3 条(P1–P2、X1) |
| **实模型演练层合计** | **10 条**(默认 SKIP,需 `REFLECT_LIVE=1`) |
| `sdk-python/test_sdk_real_bin.py` | 9 条(K1–K8) |
| `sdk-ts/real-bin.test.mjs` | 5 条 |
| `rust/cli_core.sh` | 71 断言 |
| `rust/cli_deep.sh` | 194 断言 |
| `rust/serve_ops.sh` | 42 断言 |
| `rust/tools_drive.sh` | 40 断言 |
| `rust/integrations.sh` | 44 断言 |
| `rust/cli_modules.sh` | 46 断言 |
| `rust/examples_e2e.sh` | 10 断言 |
| 另有 cargo test --workspace / clippy / fmt / doc | 既有基线 |

## 8. 未覆盖项(诚实清单)

- **TUI 交互面**(modal / 按键 / 渲染):本仓库已不含 TUI 子命令,无对象。
- **真实 LLM provider 的流式/重试/限流路径**:mock provider 覆盖协议
  语义,真实 provider 行为(429 退避、连接中断重试)需要网络,不在离线
  套件范围;`crates/abilities/reflect-llm` 内有 wiremock 单测覆盖。
- **cron / background task 的定时触发路径**:启动期可见(cron scheduler
  started),定时到期的真实触发依赖时间流逝,未做长时等待断言。
- **OTLP telemetry 真实上报**:本地文件 trace(traces 子命令)已覆盖;
  OTLP exporter 只验启动不报错。
- **并行远程工具调用**:mock provider 每次模型回复只发一个 tool_call,
  无法驱动"一个 assistant turn 内并行发多个远程工具"的路径。
- **`agent_message`(非流式 provider 的整段消息)**:mock provider 恒走
  delta 路径,该变体在离线套件不可达。
- **bash 参数面套件的 Python 化**:见 §9 迁移路线(本次未迁)。

## 9. bash 参数面 → Python 迁移路线(后续批次)

`tests/rust/*.sh` 目前是 CLI **参数面**的唯一实现(320+ 断言)。能力面
已在 Python 落地,参数面按模块逐步迁移,迁完即删对应 bash 脚本:

| 顺序 | bash 脚本 | 目标文件 |
|---|---|---|
| 1 | `serve_ops.sh` | `functional/test_serve.py`(协议面已覆盖大半) |
| 2 | `tools_drive.sh` | `functional/test_tools.py` |
| 3 | `integrations.sh` | `functional/test_orchestration.py` |
| 4 | `cli_deep.sh` | `functional/test_cli.py` |

迁移期间两侧都跑(能力面 + bash),断言重叠不算问题;迁移完成后从
`tests/rust/run.sh` 摘掉对应步骤。
