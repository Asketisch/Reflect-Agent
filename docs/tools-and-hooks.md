# 工具与 Hooks 设计规范

> 本文是 `reflect-tools` / `reflect-hooks` 两个 crate 的设计契约文档,
> 源码 doc-comment 以 `§N.M` 形式引用本文章节。行为以代码为准,本文
> 与代码不一致时视为文档 bug,欢迎修文档或提 issue。

## 目录

- §1 工具总览与注册表分层
- §2 内置工具契约
- §3 工具执行队列与并发模型
- §4 Hook 事件与决策协议
- §5 内置 Hook 与配置 schema
- §6 权限模式
- §7 审批门(ApprovalGate)
- §8 沙箱
  - §8.1 OS 级沙箱(Seatbelt / Landlock)

---

## §1 工具总览与注册表分层

`Tool` trait(`reflect-tools/src/tool.rs`)是所有工具的公共契约:
`name` / `description` / `parameters_schema`(JSON Schema 2020-12)/
`is_concurrency_safe` / `execute(ctx, args)`。

`ToolRegistry` 分三层,查找顺序 Runtime > Dynamic > Builtin:

| 层 | 注册者 | 典型内容 |
|---|---|---|
| Builtin | 框架启动 | 23 个内置工具(§2) |
| Dynamic | 编排层 | `call_<role>`(subagent)、`TaskCreate` 等 task 工具、`send_message`(discussion)、`load_skill`(skills) |
| Runtime | 集成层 | `mcp__<server>__<tool>`(MCP)、`lsp`(LSP)、`ast`、`checkpoint` / `rewind` / `cron`(exec) |

同名冲突时高层覆盖低层;`tool_search` 工具在工具面过大时按 schema
检索替代全量注入。

## §2 内置工具契约

通用约定:

- 路径参数(`file_path` / `path`)一律解析为绝对路径,拒绝 workspace 逃逸;
- `read` 前置 `FileReadStateTracker` 记录(§4.7),`edit` / `write` 前置
  `ReadBeforeEditHook`(§5)校验"先读后写";
- 工具输出统一 `ToolOutput { content, is_error, metadata, elapsed_ms }`,
  超长输出截断到 16 KiB(sanitize 之后)。

各工具细节(源码 `crates/abilities/reflect-tools/src/builtins/`):

- §2.2 `read`:offset / limit 分页;图片走 `image_view` 多模态块;
  notebook 用 `notebook_edit` 的 list / read action。
- §2.3 `write`:整文件覆写;先读后写校验;报告写入字节数。
- §2.4 `edit`:`old_string` 精确匹配 + `replace_all`;多处命中报错;
  mtime 漂移超阈值拒写(防并发覆盖)。
- §2.5 `grep`:`ignore::WalkBuilder` 遍历 + regex;`rank_mode = bm25`
  时按 BM25 重排(反射 `reflect-bm25`)。
- §2.6 `glob`:pattern / path;同样走 ignore 树(尊重 .gitignore)。

其余:执行类 `bash`(timeout_ms / run_in_background)、`background_status`;
Web 类 `web_fetch`(SSRF 防护)/ `web_search`(Brave)/ `url_safety`;
计划类 `enter_plan_mode` / `exit_plan_mode` / `plan_write`;交互类
`ask_user` / `ask_user_question` / `request_human_input`;worktree 类
`enter_worktree` / `exit_worktree`;元工具 `tool_search` / `brief` /
`get_context_remaining` / `echo` / `delete_file` / `notebook_edit` /
`image_view`。

## §3 工具执行队列与并发模型

`ToolExecutionQueue`(`reflect-tools/src/queue.rs`):

- `is_concurrency_safe() == true` 的工具(read / grep / glob / web 类)
  并发执行;unsafe 工具(bash / write / edit)**按提交顺序串行**,
  防止写写冲突;
- 队列消费 hook 决策:PreToolUse 的 `Deny` 直接转错误结果、
  `ModifyArgs` 替换入参、`PermissionOverride` 切换权限模式、
  `Ask` 路由到 ApprovalGate(§7);
- 取消:`CancellationToken` 级联(bash 子进程 kill、流中断)。

## §4 Hook 事件与决策协议

### §4.1 事件(12 类,`reflect-hooks/src/event.rs`)

| 事件 | 触发点 | hook 可否决 |
|---|---|---|
| `PreToolUse` | 工具执行前 | 是(Deny / ModifyArgs / PermissionOverride / Ask) |
| `PostToolUse` | 工具成功后 | 否(审计) |
| `PostToolUseFailure` | 工具失败 / 超时后 | 否 |
| `Stop` | turn 结束前 | 是(Deny 强制续跑) |
| `SessionStart` | 会话启动,每线程一次 | 否 |
| `SessionEnd` | 会话结束(Op::Shutdown 或通道关闭),v1.6 | 否 |
| `UserPromptSubmit` | prompt 进模型前 | 是(Deny 拒整回合) |
| `PreCompact` | 压缩执行前 | 是(Deny 跳过本轮压缩) |
| `PostCompact` | 压缩完成后,v1.6 | 否(审计 / 恢复钩子) |
| `TaskCreated` / `TaskUpdated` / `TaskCompleted` | 任务板生命周期 | 否 |

`HookEvent` 不携带完整 `ToolContext`(避免 tools↔hooks 循环依赖),
需要上下文的事件提供精简 `HookContext { session_id, turn_id, workspace,
permission_mode }`。

### §4.2 决策(7 种,`decision.rs`)

`Allow` / `Deny { reason }` / `ModifyArgs(json)` /
`InjectMessage(SystemMessage)` / `PermissionOverride(PermissionMode)` /
`Ask { reason }`(转人工审批模态)/ `Combined(Vec)`(单 hook 返回多重
决策)。§4.5:`Combined` 经 `ResolvedDecision::resolve()` 规范化展平,
冲突时 **Deny > ModifyArgs > InjectMessage > Allow** 优先。

### §4.4 引擎合并优先级

`HookEngine::dispatch` 按注册顺序依次调用订阅了该事件的 hook,
合并规则:任一 Deny 即终局 Deny;多个 ModifyArgs 后者覆盖前者;
多个 InjectMessage 拼接;多个 Ask 以首个为准(避免连环弹窗)。

### §4.7 读状态跟踪(file_read_state.rs)

`FileReadStateTracker` 记录"哪些文件在何时以何 mtime 被读过",
供 `ReadBeforeEditHook`(§5)判定 edit/write 的 old_string 是否基于
新鲜内容;`DenyReason` 区分"从未读" / "已过期(mtime 漂移)"。

## §5 内置 Hook 与配置 schema

内置 hook(`reflect-hooks/src/builtins/`):

- §5.1 `SearchBudgetHook`:单 turn 内 grep/glob 次数预算,超限注入提醒。
- §5.2 `TestRunnerHook`:PostToolUse(写类工具)后自动跑测试命令。
- §5.3 `PlanCompletionHook`:plan 模式下跟踪 `plan_write` 完成度。
- §5.4 `VerificationHook`:Stop 时跑验证命令,失败 Deny 强制续跑修复。
- `PlanModeGateHook`:plan 模式下 blanket-deny 写工具(见 PLAN_MODE.md)。
- `ReadBeforeEditHook`:先读后写强制(§4.7)。
- `LangfuseTrackerHook`:遥测埋点。

§5.5 配置 schema(`hooks.json` / config.toml `[hooks]`):数组,每项
`{ name, event, matcher?, command, timeout_ms? }`;`event` 为
§4.1 事件名的 snake_case;`matcher` 仅对带工具名的事件生效,支持
`*` / `?` 通配。`ShellHook`(shell.rs)把事件 JSON 写子进程 stdin,
stdout 回 JSON 决策(`{"decision":"deny","reason":"..."}` 等);
**fail-closed**:命令超时或非零退出视为 Deny。

## §6 权限模式

7 级(定义于 reflect-protocol):`Auto` / `Prompt` / `Deny` / `Plan` /
`AcceptEdits` / `Bubble`(非阻塞通知)/ `Bypass`(静默全批)。
规则引擎见 `reflect-permissions`(tool 精确名 + tool_glob + bash
shell_pattern,匹配上下文感知);bash 命令经 `classify_command` 三级
分类(Safe / Risky / Dangerous)供审批分级。

## §7 审批门(ApprovalGate)

`reflect-tools/src/approval/gate.rs`:`ask` 发出
`EventMsg::ApprovalRequest`,等待 `Op::ToolApproval` 回执;支持
ApproveForSession(会话内记忆)。hook 的 `Ask` 决策同样路由到此处
(`ApprovalKind::Hook`)。

## §8 沙箱

### §8.1 OS 级沙箱(Seatbelt / Landlock)

`reflect-sandbox/src/os_sandbox.rs`:macOS Seatbelt(profile 级策略,
workspace-write 默认)/ Linux Landlock(路径白名单)。bash 工具的
`sandbox` 参数启用;策略与权限模式正交(见 §6)。网络隔离目前仅
`web_fetch` 层实施(SSRF 防护:私有网段 / 逐跳 redirect 校验)。
