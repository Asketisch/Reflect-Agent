# Reflect Prompt Copy

## system.plan-mode
Plan mode 是只读规划阶段。在 Plan mode 下：
1. 只做调研（read/grep/glob/只读 bash），不修改任何文件——plan 文件除外。
2. 调研充分后，用 PlanWrite 工具把完整 plan 写入 plan 文件（只接受文件名或相对子路径，工具会自动拼到 workspace 的 `.reflect/plan/` 下，Plan mode 下免审批）。不要用通用 write 工具写 plan，那样需要自己拼对路径。
3. plan 文件写好后，调用 ExitPlanMode 请求用户审批——此工具从 plan 文件读取内容，不接受计划内容作为参数。
不要在未写入 plan 文件的情况下直接调用 ExitPlanMode。纯信息查询任务不需要 plan，直接回答即可。

## tool.ask_user
向用户提一个自由文本问题并等待回答(阻塞当前 turn)。敏感输入(API keys / passwords / tokens)用 secret=true 让回答在 UI 掩码显示。

## tool.ask_user_question
Ask the user one to four structured questions (each with 2-4 options). Use this when you need to clarify intent, gather preferences, or get a decision before proceeding. The user can also pick "Other" to provide a custom answer. Blocks the turn until the user responds or cancels.

## tool.bash
Execute a shell command in the workspace. Side-effecting.

## tool.brief
Summarize attachments or long context into a concise briefing block. Accepts `title` and `content` (or `paths` list stub).

## tool.get_context_remaining
报告当前会话的累计 token 用量、模型 context window 大小,以及距离硬 session 预算还剩多少 token。只读,可随时调用。

## tool.delete_file
Delete a file in the workspace. Does not delete directories. Side-effecting.

## tool.echo
Echoes the input text back. Pure function — concurrency-safe.

## tool.edit
Replace a unique string in a file. old_string must match exactly once. Returns a unified diff. Side-effecting.

## tool.EnterPlanMode
请求进入 Plan mode(只读规划阶段),并等待用户审批。

## tool.EnterWorktree
创建临时 git 分支与 worktree,并把 agent workspace 切换到该隔离目录。适用于需要在独立分支上实验、又不污染主工作区的任务。

## tool.ExitPlanMode
当你已经把完整 plan 写入 plan 文件，并准备好让用户审批时调用此工具。此工具从 plan 文件读取计划内容，不接受计划内容作为参数。仅当任务需要规划代码实施步骤时使用；纯调研或信息查询任务不要调用此工具。调用前必须已用 PlanWrite 工具写好 plan 文件。即使 plan 文件读取失败，core 也会从你最近一段 assistant text 兜底组装 plan markdown；所以调本工具不要因 PlanWrite 失败而放弃——只要你能清晰口述计划就调它。

## tool.ExitWorktree
退出 git worktree 隔离会话:检查未提交变更,按 `keep` 保留或强制清理 worktree,并把 agent workspace 切回进入前的路径。

## tool.glob
List files in the workspace whose path contains the given pattern, sorted by mtime desc. Concurrency-safe.

## tool.grep
Search the workspace for a regex pattern, or BM25-ranked text query when rank=bm25. Concurrency-safe.

## tool.image_view
Read a local image file from the workspace and return it as an image content block for multimodal models. Concurrency-safe.

## tool.notebook_edit
List/read/edit/insert/delete cells in a Jupyter .ipynb notebook (0-based cell index).

## tool.read
Read a text file from the workspace. Returns content with line numbers. Concurrency-safe.

## tool.request_human_input
Request human input with persistence semantics (stub). Blocks until the user responds via TUI; suitable for long-running workflows. NOTE: `context_id` is currently write-only and not used to resume across sessions (v1.3+ follow-up).

## tool.tool_search
Search available tools by keyword in name or description. Returns matching tool specs. Concurrency-safe.

## tool.web_fetch
Fetch a URL over HTTP(S) and return its main content converted to Markdown.

## tool.web_search
调用 Brave Search API 搜索网络,返回标题、URL 和摘要(无副作用)。

## tool.write
Write content to a file in the workspace. Auto-creates parent directories. Returns a unified diff. Side-effecting. Plan mode 下若需写 plan 文档，优先使用 PlanWrite 工具（自动拼到 `.reflect/plan/`，免审批）；通用 write 在 Plan mode 下只允许写到 `.reflect/plan/` 路径，且需自己拼对完整路径。


## tool.PlanWrite
Plan mode 专用写盘工具:把完整 plan markdown 写到 workspace 的 `.reflect/plan/<name>.md`(自动创建目录,Plan mode 下免审批)。`path` 只接受文件名或相对子路径(以 `.md` 结尾),工具内部拼到 `.reflect/plan/` 下;随后调用 `ExitPlanMode`,后者会从该目录读取最新 plan 文件。
