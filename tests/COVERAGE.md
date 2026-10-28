# 功能覆盖矩阵(测试 ↔ 功能面映射)

本矩阵回答一个问题:**当前测试是否完整覆盖了全部对外功能**(而非冒烟)。
覆盖口径:每条功能面至少有一个**字段级/行为级断言**(非"退出码 0")的
用例;断言失败即失败。全程离线(`ZHUYE_MODEL=mock`)+ 隔离 HOME。

## 0. 覆盖分层:谁测什么

| 层 | 位置 | 测谁 | 依赖 SDK? | 规模 |
|---|---|---|---|---|
| **能力覆盖层**(主) | `tests/functional/` | **Rust 代码在真产物上的行为**(原始 JSONL wire) | ❌ | 54 条 |
| CLI 参数面 | `tests/rust/*.sh` | 17 个子命令的参数矩阵与输出契约 | ❌ | 320+ 断言 |
| SDK 绑定层 | `tests/sdk-python/`、`tests/sdk-ts/` | 两个**客户端自己**的行为 | ✅ | Python 10 条 / TS 10 条 |
| SDK 单测 | `sdks/*/tests/` | SDK 纯逻辑(mock serve),随包发布 | ✅ | Python 15 / TS 18 条 |
| crate 单测 | `crates/**/tests/` | 内部单元与集成 | — | 既有 |
| **实模型演练层**(可选) | `tests/live/` | **真实模型**下的引擎行为(协议形状) | ❌ | 12 条 |

**新增能力断言只落能力覆盖层一处** —— 不要在 SDK 测试里重复写协议/能力断言。
理由:SDK 自身的 bug 会把 Rust 的 bug 遮住(见 §6 已修复项 D1)。

⚠️ **实模型演练层不算能力覆盖** —— 它断言的是**协议形状与运行时行为**
(事件序 / 终态 / usage 字段 / 工具成对),**不断言具体回答文本**。
能力断言仍然只在 `tests/functional/`。两层不重叠、不互相替代:
能力覆盖层是 CI 强制门禁(离线、确定性),实模型层是可选手动/夜间门禁
(需外网与免费额度,默认 SKIP)。

## 1. 能力 × 覆盖层矩阵

| 能力域 | 能力覆盖层 | CLI 参数面 | SDK 绑定层 | crate 单测 | 实模型层 |
|---|---|---|---|---|---|
| 基础通信(握手/单轮/多轮/事件流/终态/工具参数增量) | D1–D5、D9 | cli_core | K1–K2、K5 | ✅ | L1–L4 |
| 并发与路由 | S1–S3 | — | — | — | — |
| 内置工具调用与 OS 沙箱 | T1–T2、T8–T10 | tools_drive | — | ✅ | T1–T3 |
| 远程工具(注册/往返/失败/超时) | T3–T4、X4 | serve_ops | K3、K8 | ✅ | — |
| 审批交互(approve / deny / bubble / shell 规则) | T5–T7 | integrations | — | ✅ | — |
| 会话状态 Op(19 Op) | A1–A12 | serve_ops | K4、K9 | ✅ | — |
| 重连与恢复(resume / -c / -r / 崩溃 / EOF) | R1–R5 | cli_core(exec --resume) | — | ✅ | P2 |
| 容错与失败恢复 | X1–X6 | — | K7 | ✅ | X1 |
| 持久化(rollout / 截断 / 索引) | P1–P5 | cli_deep(session) | — | ✅ | P1–P2 |
| 编排(子代理/讨论/任务/团队/流水线/goal) | O1–O5 | cli_modules、cli_deep | — | ✅ | — |
| CLI 能力面 | C1–C6 | cli_deep(194 断言) | — | — | — |
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
- **D9 工具参数增量**(v1.5 additive 变体):模型生成工具参数期间逐段转发
  `tool_call_args_delta`;字段级断言 `call_id` + `delta` 非空、分片按到达
  顺序拼接等于完整参数 JSON、`call_id` 与 `tool_call_begin` 一致、时序在
  `tool_call_begin` 之后。夹具用 mock 的 `tool_call_delta` 脚本行制造
  **分片形态**参数下发(整包形态没有 delta 可转发)。

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
- **T5** 工具审批全链路(`ZHUYE_APPROVALS=1` + MCP 工具):`approval_request`
  (request_id / kind.tool_name / risk)→ 回 `tool_approval` → 工具真执行 →
  `turn_complete`。
- **T6** 审批 deny → 工具**未执行**、`tool_call_end(is_error=true, 含 deny 理由)`、
  turn 仍收尾。
- **T7** Bash 显式拒绝规则覆盖换行、后台执行附加命令(错误字段与无副作用)。
- **T8** OS 沙箱拒绝 workspace 外写入时,错误标记与非零退出码正确返回。
- **T9** macOS Seatbelt 按真实 HOME 拦截假密钥读取,不把星号当成 subpath 通配符。
- **T10** stdout/stderr 同时截断后,`tool_call_end` 仍返回失败、原始退出码与捕获字节数。

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
rewind 物理截断(记录减少 + `.rewind-<ts>.bak` 备份)/ `zhuye session ls`
能列出该会话。P5 覆盖旧日期会话通过 thread id、内部 session id、`-c`、`-r 1` 恢复后续写原文件。

### O 组 编排(`test_orchestration.py`)
子代理 `call_explorer`(子代理执行 → 回执进历史)/ 讨论编排(transcript +
Result + rounds_completed)/ 任务与团队(create → ls)/ 流水线错误路径
(no runner / team not found)/ goal 模式与其他 op 共存。

### C 组 CLI 能力面(`test_cli.py`)
`serve` 三个恢复入口 / `exec` 输出每行是合法 JSON 且含 turn 事件 /
`session ls` + `show` 报错路径 / `doctor` + `version` 输出契约 /
`session search` 跨会话全文搜索(命中 / 未命中 / `--json` 字段)/
`exec` 退出码映射(成功 0 / turn 失败 1 / plan mode 未产出计划 122)。

## 3. CLI 子命令(17 个,不含 help)

| 子命令 | 冒烟(cli_core/cli_modules) | 深度断言位置 |
|---|---|---|
| `exec` | ✅ | cli_deep(exec JSONL 字段级)、tools_drive(全工具驱动)、能力层 C2/C6(退出码映射) |
| `serve` | ✅ | **能力层 19 Op 全量** + serve_ops.sh(FIFO 驱动,逐 op 断言 EventMsg)+ integrations(MCP/bubble/热重载) |
| `login` | ✅ | cli_deep:4 provider 路径、缺 key 报错、短 key 告警、`--force`、api_key 脱敏 |
| `mcp` | ✅ | cli_deep:stdio+http 双传输 add/互斥报错/ls/show/remove/`test`(真 mock server 成功+坏 binary 失败)/registry 三路径 |
| `config` | ✅ | cli_deep:set/unset/ls/show/`--reveal`/非法 key 报错 |
| `session` | ✅ | cli_deep:双 ID 前缀解析回归(文件名前缀+内部 id 前缀+歧义报错)/rename/export/fork/rm/search;能力层 P4/C3/C5 |
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
- **notes**:`add_session_note` → `~/.zhuye/session-notes/<thread>.jsonl`
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
  headless 下只验孤儿回执的 wire 级健壮性。serve + `ZHUYE_APPROVALS=1`
  可触发真实 gate(能力层 T5/T6 已覆盖)。
- `web_search` / `web_fetch` 离线时优雅报错(无 provider key / 无网络)。
- **并发 turn 是真并发**:core 对每个 `UserInput` 提交 `tokio::spawn` 一个
  独立 task(`submission_loop.rs:621`),两个 turn 的模型调用先后不保证 ——
  已与 `sdks/PROTOCOL.md` §4 的并发语义对齐(见 §6 D5)。
- **三个 id 互不相同**:`session_configured.session_id`、rollout 内
  `session_meta.session_id`、会话**文件名**里的 thread id 是三个不同的
  uuid(exec/serve 路径未注入 `with_session_id`)。`--resume` 支持**文件名**
  thread id 与 SessionMeta 内部 id;`zhuye session ls` 显示后者(P5 覆盖双 ID)。

## 6. 已知缺陷与修复状态

### 已修复或经复核排除

- D1:Python `close()` 在写锁内发送 shutdown,禁止后续写入;mock 区分 shutdown 与仅 EOF 退出,验证关闭协议与幂等性。
- D2/D3:双 SDK 的 turn 状态、结构化中止原因、plan 审批选择与 Python 审批 wire 类型已与 Rust 对齐。
- D4:Python 新增 `next_global_event(timeout_secs)` 公开读取口;K9 验证绑定层全局事件路由,逻辑测试覆盖超时与 EOF 重复读取。
- D5:协议文档改为并发 turn 与按 submission id 路由。
- D6:此前已修正测试断言并纳入 CI;本轮审批用例改为等待 MCP 就绪事件,权限轨迹按记录的 `at` 时间戳还原,避免误把异步落盘顺序当成状态变化顺序;examples 冒烟逐一检查全部六个产物,缺失任一个时重新构建,构建失败计入失败;TS mock 测试为每个 SDK 实例提供独占流,未知事件断言检查实际透传字段,避免争抢握手造成未处理拒绝;配额持久化用例迁入独立集成测试进程,隔离全局路径变量与其他 tracker 的并发写入。
- D7:流式输出分支保留内部状态行用于退出码解析,只从增量事件中过滤它;T8/T9 已验证 OS 沙箱拒绝后的 `is_error=true` 与非零退出码;T10 验证双流截断、状态行丢失后仍回退到原命令退出码。
- D8:当前 `classify_action` 对 `InvalidRequest` 返回 `GiveUp`,不会进入构造冷却原因的分支;旧记录所述一小时冷却不成立。
- D10:双 SDK 新增 `ToolCallOutputDelta` 类型,逻辑测试覆盖输出增量字段透传及未知变体兼容。
- D11:TS SDK 新增 `nextGlobalEvent` 公开读取口(此前 `id=""` 全局事件被静默丢弃),并补齐 K5/K7/K7b/K8/K9 真二进制用例与 Python 侧对齐;TS `prompt()` 遇 `turn_aborted` 改为抛错(不再静默返回部分文本);Python 补 `SubmissionClosed` 类型声明;TS stdout EOF 现在会唤醒挂起的迭代器(对齐 Python `_on_eof`);发布名落定为 `zhuye`,`__version__` 纳入四处版本联动。
- D12:SDK 随包分发平台二进制(参考 Codex / ruff 模式)—— npm 主包按平台安装 `zhuye-<slug>` 子包(optional dependencies,清单由 CI 动态生成),Python 切 maturin 出平台 wheel(二进制以 `zhuye/bin/` 包数据单份携带);双 SDK 二进制解析链统一为 `options.bin → ZHUYE_BIN → 内嵌二进制 → PATH`,平台不匹配回退 PATH;安装冒烟与发布后 registry 验证升级为断言内嵌二进制 `--version` 可执行。
- Python 3.10 兼容性:移除 Python 3.11 才提供的 `typing.NotRequired` 依赖,用 `TypedDict` 继承区分必填与可选字段,保持零运行时依赖。

### 实模型层观测到的上游限制(非 Zhuye 缺陷)

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
| `functional/test_serve.py` | 22 条(D6 + S3 + A13) |
| `functional/test_tools.py` | 11 条(T1–T10) |
| `functional/test_recovery.py` | 11 条(R5 + X6) |
| `functional/test_persistence.py` | 8 条(P1–P5) |
| `functional/test_orchestration.py` | 5 条(O5) |
| `functional/test_cli.py` | 6 条(C6) |
| **能力覆盖层合计** | **63 条** |
| `live/test_live_basic.py` | 4 条(L1–L4) |
| `live/test_live_tools.py` | 3 条(T1–T3) |
| `live/test_live_persistence.py` | 3 条(P1–P2、X1) |
| **实模型演练层合计** | **10 条**(默认 SKIP,需 `ZHUYE_LIVE=1`) |
| `sdk-python/test_sdk_real_bin.py` | 10 条(K1–K9) |
| `sdk-ts/real-bin.test.mjs` | 10 条(K1–K9,与 Python 侧对齐) |
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
  套件范围;`crates/abilities/zhuye-llm` 内有 wiremock 单测覆盖。
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
