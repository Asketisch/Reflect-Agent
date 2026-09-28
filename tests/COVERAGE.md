# 功能覆盖矩阵(测试 ↔ 功能面映射)

本矩阵回答一个问题:**当前测试是否完整覆盖了全部对外功能**(而非冒烟)。
覆盖口径:每条功能面至少有一个**字段级/行为级断言**(非"退出码 0")的
用例;断言失败即失败。全程离线(`REFLECT_MODEL=mock`)+ 隔离 HOME。

## 1. CLI 子命令(17 个,不含 help)

| 子命令 | 冒烟(cli_core/cli_modules) | 深度断言位置 |
|---|---|---|
| `exec` | ✅ | cli_deep(exec JSONL 字段级)、tools_drive(全工具驱动)、serve 侧另测 |
| `serve` | ✅ | **serve_ops.sh 19 Op 全量**(FIFO 驱动,逐 op 断言 EventMsg)+ integrations(MCP/bubble/热重载) |
| `login` | ✅ | cli_deep:4 provider 路径、缺 key 报错、短 key 告警、`--force`、api_key 脱敏 |
| `mcp` | ✅ | cli_deep:stdio+http 双传输 add/互斥报错/ls/show/remove/`test`(真 mock server 成功+坏 binary 失败)/registry 三路径 |
| `config` | ✅ | cli_deep:set/unset/ls/show/`--reveal`/非法 key 报错 |
| `session` | ✅ | cli_deep:双 ID 前缀解析回归(文件名前缀+内部 id 前缀+歧义报错)/rename/export/fork/rm |
| `traces` | ✅ | cli_deep:ls/show 前缀/不存在报错 |
| `doctor` | ✅ | cli_deep:常规 + `--check-network`(mock provider DNS 失败仍 rc=0) |
| `plugin` | ✅ | cli_deep:marketplace add/ls/refresh/remove + 安装 + info/show/enable/disable/uninstall 全链路 |
| `lsp` | ✅ | cli_deep:空配置提示/未配置 status 报错/配置后 list+status |
| `task` | ✅ | cli_deep:create/ls/show/update/stop/purge + team 全链路 + 非数字 id clap 报错 |
| `pipeline` | ✅ | cli_deep 错误路径(team 不存在/自定义 label 无 runner —— 产品限制,见 §5) |
| `discussion` | ✅ | cli_deep:mock 驱动真多 agent(sequential,transcript 非空)+ `--output` |
| `security` | ✅ | cli_modules(cargo audit 封装,断言输出结构) |
| `workspace` | ✅ | cli_deep:ls/clone 本地仓库/`--depth` 浅克隆/坏 URL 报错 |
| `update` | ✅ | cli_deep:常规 + `--check-only` |
| `version` | ✅ | cli_core(断言版本号格式) |

## 2. serve 协议 Op 面(19/19)

`serve_ops.sh`(42 断言):`user_input`(turn 全事件流 + thread_settings
4 字段透传)/ `set_effort`(静默,仅收尾标记)/ `set_permission_mode` /
`cycle_permission_mode`(精确 from→to)/ `rewind` / `interrupt` /
`compact` / `enter_goal_mode` / `exit_goal_mode` / `tool_approval` /
`hook_approval`(孤儿回执不崩)/ `ask_user_question_response` /
`ask_user_input_response`(孤儿回执)/ `enter_plan_mode` /
`exit_plan_mode`(fallback markdown + 落盘)/ `plan_approval`
(auto_mode 切权限模式 / revise → plan_rejected)/ `register_tools` /
`tool_execution_response`(成功+失败双向回执)/ `shutdown`。

SDK 侧同一 Op 面经 `submit_op` 再验一遍:
`test_sdk_ops.py` / `ops.test.mjs`(非 turn ops 全部走 `submission_closed`
收尾,plan ops 断言 plan_request/plan_ready/plan_id)。

## 3. 内置工具(22 个内置 + 运行时工具)

`tools_drive.sh`(40 断言,exec 驱动):echo / bash / read / write / edit /
delete_file / grep / glob / ast / get_context_remaining / tool_search /
brief / image_view / notebook_edit / web_fetch(离线优雅报错)/
web_search(离线优雅报错)/ EnterWorktree / ExitWorktree / checkpoint /
rewind / ask_user / request_human_input(headless 优雅报错)。
另有专项:
- **read_before_edit hook 四态**:新建 Allow / 已存在未读 Deny / 读后未改
  Allow / mtime 漂移 Deny。
- **bash OS 沙箱**:workspace 内写成功,workspace 外写被 Seatbelt 拒绝。
- **Plan 工具**(EnterPlanMode/ExitPlanMode/PlanWrite)走 serve 路径在
  serve_ops.sh 覆盖(exec 路径阻塞等 plan_approval,不驱动)。

## 4. 集成面

`integrations.sh`(44 断言):
- **MCP**:server 生命周期(`mcp_server_started` 含 tool_names/tool_count、
  坏 server → `mcp_server_failed`)+ in-turn 调用(`mcp_tool_invoked` +
  `tool_call_end` 回传 echo 输出)。
- **skill**:工作区 `SKILL.md` 扫描 + `load_skill` 激活(正文 + 激活工具
  列表);不存在的 skill 优雅报错。
- **notes**:`add_session_note` → `~/.reflect/session-notes/<thread>.jsonl`
  落盘(内容 + JSON 合法性)。
- **bubble 审批**:Bubble 模式下 Prompt 权限工具 → `permission_bubble`
  非阻塞通知 + 自动放行执行。
- **配置热重载**:`config_reloaded`(path + sections_changed)+ 重载后会话存活。
- **plugin**:marketplace add/install/enable → 启动期 `plugin_loaded`
  (plugin 名/version/skill_count)。

SDK 审批面(`test_sdk_ops.py` / `ops.test.mjs`):
- **审批全链路**:MCP 工具(Auto 模式必经 gate)→ `approval_request`
  (request_id/kind/tool_name)→ SDK `approve(id, "approve")` → 工具真执行
  → `turn_complete`;孤儿回执不崩、会话存活。
- **rollout 持久化**:`~/.reflect/sessions/YYYY/MM/DD/<thread>.jsonl` 含
  `session_meta` / `message` / `permission_mode_changed`(from→to 轨迹 +
  at 时间戳)。

## 5. 已知产品限制(测试断言当前行为,防退化)

- ~~`pipeline run`:plan 预设模板硬编码 `{{input.audience}}`~~(v1.6 已修复:
  planner 预设只依赖 `{{topic}}`,CLI `--input k=v` 注入模板 inputs,
  build_factory 从配置引导 registry 使 mock 可离线命中);
  `pipeline team` 失败节点存在时退出非 0(v1.6 对齐 CI 语义)。
- `discussion run`:concurrent 模式受 SubAgentFactory 深度上限约束;
  无 provider 时降级 `run_noop`(退出 0,Result JSON 仍含 outcome)。
- `approval_request` / `ask_user*` 的**交互式工具路径**需要 TUI/modal;
  headless 下只验孤儿回执的 wire 健壮性。serve + `REFLECT_APPROVALS=1`
  可触发真实 gate(SDK 深度用例已覆盖)。
- `web_search` / `web_fetch` 离线时优雅报错(无 provider key / 无网络)。

## 6. 审计中发现并修复的问题(7 个)

| # | 缺陷 | 修复 | 守门用例 |
|---|---|---|---|
| 1 | CLI 未注册 read/write/edit/grep/glob 等内置工具 | `crates/runtime/reflect-cli` 补注册 | tools_drive(逐工具驱动) |
| 2 | `session show` 只认文件名前缀,内部 id 前缀解析失败 | session.rs 双前缀解析 + 歧义报错 | cli_deep(session 组) |
| 3 | `mcp_tool_invoked` 事件定义了但无 emit 路径(死事件) | `McpToolAdapter::execute` 完成后 emit(bootstrap 注入 event_tx) | integrations(mcp 组) |
| 4 | 热重载 task 循环前预调 `rx.changed()` 吞掉启动后第一次真实变更 | 删除预调守卫,直接进循环 | integrations(hotreload 组) |
| 5 | 非 turn op 无终态事件 → SDK `submit_op` 迭代器**永久挂起** | 新增 `EventMsg::SubmissionClosed`(serve per-turn 通道排空后发出,挂 sub id);两 SDK 终态集 + PROTOCOL.md 同步 | serve_ops(set_effort 静默断言)+ 两 SDK ops 用例 |
| 6 | TS SDK `approve` 发 `{type:'approve'}`,与 wire 的 `ReviewDecision`(`"approve"`/`"approve_for_session"`/`{"deny":{...}}`)不符 → 反序列化失败,审批永久挂起 | TS SDK 透传 wire 值 + 协议类型改扁平结构;PROTOCOL.md 同步 | 两 SDK ops 用例(MCP 审批全链路) |
| 7 | `find_file_by_session_id` 的 active/fallback 分支触发 clippy `if_same_then_else`(`-D warnings` 全量构建失败) | 合并为单一条件 `if active \|\| found.is_none()`(语义等价) | run_all 的 clippy 步骤 |

## 7. 用例规模汇总

| 文件 | 断言/用例数 |
|---|---|
| `rust/cli_deep.sh` | 194 断言 |
| `rust/serve_ops.sh` | 42 断言 |
| `rust/tools_drive.sh` | 40 断言 |
| `rust/integrations.sh` | 44 断言 |
| `sdk-python/test_sdk_real_bin.py` | 7 用例 |
| `sdk-python/test_sdk_ops.py` | 4 用例 |
| `sdk-ts/real-bin.test.mjs` | 6 用例 |
| `sdk-ts/ops.test.mjs` | 4 用例 |
| 另有 cli_core / cli_modules / examples_e2e / cargo test --workspace / clippy / fmt / doc | 既有基线 |

## 8. 未覆盖项(诚实清单)

- **TUI 交互面**(modal / 按键 / 渲染):本仓库已不含 TUI 子命令,无对象。
- **真实 LLM provider 的流式/重试/限流路径**:mock provider 覆盖协议
  语义,真实 provider 行为(429 退避、连接中断重试)需要网络,不在离线
  套件范围;`crates/abilities/reflect-llm` 内有 wiremock 单测覆盖。
- **cron / background task 的定时触发路径**:启动期可见(cron scheduler
  started),定时到期的真实触发依赖时间流逝,未做长时等待断言。
- **OTLP telemetry 真实上报**:本地文件 trace(traces 子命令)已覆盖;
  OTLP exporter 只验启动不报错。
