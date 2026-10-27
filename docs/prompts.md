# 提示词库(v1.6):外置 + 按模型匹配

烛夜的所有 LLM 提示词从 v1.6 起外置为 Markdown,并按**当前模型**
动态匹配 —— 同一个 agent 换用不同 LLM(如 claude / gpt / gemini /
deepseek)时,可以加载各自调优过的提示词版本,让每个模型发挥最大能力。

## 目录约定

```text
~/.zhuye/prompts/          # 用户全局层(所有 workspace 生效)
<workspace>/.zhuye/prompts/  # workspace 层(同名文件覆盖全局层)
├── default.md               # 兜底层:未命中任何模型文件时的文案
├── claude.md                # 厂商层:spec 前缀为 claude/ 时叠加
├── gpt.md
├── gemini.md
├── deepseek.md
├── kimi.md                  # 以下为 v1.6.1 整合的厂商层
├── qwen.md
├── grok.md
├── glm.md
├── minimax.md
└── claude-sonnet-4-5.md     # 精确层:model 段完全一致时叠加
```

每个文件用 `## key` 分节,一节一条提示词。**只需写出要覆盖的 key**,缺失的
key 落回编译期内嵌默认(开箱即用,渐进调优)。

### 内置厂商层(v1.6.1 整合)

编译期内嵌了 9 个厂商层的真实覆盖文案,提炼自各厂商官方 agent system
prompt(来源与许可见各 `prompts/<vendor>.md` 文件头注释):

| 厂商层 | 来源 | 说明 |
|--------|------|------|
| claude | Claude Code Sonnet 5.5(leaks) | Harness/上下文管理/子代理(Explore/Plan)文案 |
| gpt | Codex GPT-5.6(leaks) | 编辑约束/自治边界/破坏性操作守则 |
| gemini | Gemini CLI(leaks) | 上下文效率/工具并行与确认协议 |
| kimi | Kimi K3(leaks) | 沟通/搜索与时效信息守则 |
| qwen | Qwen3.8 Max(leaks) | 函数调用格式/时效基准 |
| grok | Grok 4.7 CLI(leaks) | 危险动作/工作策略/沟通守则 |
| glm | Z.ai Code(tools 仓库) | 工作规则/UI 与代码风格;GLM 模型本身无 system prompt |
| minimax | MiniMax M2.5(leaks) | 先规划再动手/Web 项目验证/交付清单 |
| deepseek | (无正文来源) | 源只有工具 JSON 框架,层为空,全部落回 default |

内嵌厂商层跟随匹配链自动生效:`anthropic/claude-*`、`claude-*`(无前缀)
都命中 `claude.md`;`moonshot/kimi-*`、`kimi-*` 都命中 `kimi.md`;以此类推。
用户目录同名文件可整体覆盖任一厂商层。

## 匹配链

`model_call` 每次发起请求前,用当前 model spec(`provider/model`)做三级回退:

1. **精确模型名**:`claude-sonnet-4-5.md` —— model 段完全一致;
2. **厂商**:`claude.md` —— 两条命中路径并存:模型名关键词推断家族层
   (如 `claude-*` 命中 `claude.md`,无论 spec 是否带 `anthropic/` 前缀)
   与 spec 显式前缀自建层(如 `stub/any-model` 命中 `stub.md`);
   两者同时存在时,显式 provider 层胜出;
3. **default**:`default.md` —— 兜底。

三层逐级叠加,同一 key 深者胜。运行中 `/model` 热切换模型后,**下一个
请求立即使用新模型的提示词**(与模型热重载语义一致)。

旧版单文件 `~/.zhuye/prompts.md` 继续兼容(视作 default 层覆盖,
启动时打 deprecation 日志);建议迁移为 `~/.zhuye/prompts/default.md`。

## `/prompt` slash:运行中查看与刷新提示词

TUI/GUI(`/prompt` 命令,协议级 `Op::PromptCommand`)可在会话运行中
管理提示词层,无需重启:

```text
/prompt            # 列出可用层 + 当前生效层(应答 EventMsg::PromptList)
/prompt list       # 同上
/prompt reload     # 重扫提示词库,让 .zhuye/prompts/*.md 的修改即时生效
                   # (应答 EventMsg::PromptSwitched;下一请求即用新文案)
```

- 可用层 = 内置 9 个厂商层 + 用户目录层(default / 自建层),字典序去重;
- 「生效层」按当前 model spec 经匹配链解析,内嵌层不记录、只报用户层;
- headless 同样支持:`zhuye exec "/prompt reload"`(JSONL 输出
  `prompt_switched` 事件);
- 编程接口:TUI/GUI 直接提交 `Op::PromptCommand { action }`(Rust:
  `zhuye::Op`;Python 绑定按 Submission JSONL 构造),事件经
  per-submission 通道送达,`SubmissionClosed` 收尾。

## 提示词 key 一览

| 分类 | key | 用途 |
|------|-----|------|
| 主 agent | `system.main` | 默认 agent system prompt(exec 入口) |
| 主 agent | `system.facade-main` | 库门面(Builder)默认 agent prompt |
| 模式 | `system.plan-mode` | Plan mode 规则手册(compose_core 注入) |
| 模式 | `runtime.active-mode` | Plan mode「当前状态」告知段 |
| 工具 | `tool.<name>` | 内置工具 description,如 `tool.read` / `tool.bash`(共 23 个) |
| 运行时 | `runtime.nudge-loop` | 重复调用工具时的 loop 提醒 |
| 运行时 | `runtime.nudge-final` | 迭代预算耗尽的收口提醒 |
| 运行时 | `runtime.nudge-web-fetched` | web_fetch 进度摘要标题 |
| 运行时 | `runtime.nudge-web-review` | web_fetch 进度摘要收尾 |
| 运行时 | `runtime.nudge-domain-repeat` | 同域名重复抓取提醒 |
| 压缩 | `compact.full` | 全量对话摘要 prompt(`{{ conversation }}`) |
| 压缩 | `compact.recent` | 增量摘要 prompt(`{{ previous_summary }}` / `{{ recent_conversation }}`) |
| 压缩 | `compact.memory-extract` | 记忆抽取 prompt(`{{ conversation }}`) |
| 编排 | `coordinator.system` | task coordinator 默认 prompt(`[coordinator].system_prompt_path` 显式文件仍最优先) |
| 编排 | `team.lead` | team lead 成员默认 prompt |
| 编排 | `goal.verifier` | 目标模式校验器 system prompt |
| 编排 | `discussion.judge` | 讨论裁判 system prompt |
| 编排 | `discussion.participant` | 讨论参与者 user prompt 格式(`{{ topic }}` 等变量) |
| 子代理 | `subagent.explorer` | explorer 兜底 spec prompt |

带 `{{ var }}` 的 key 经 minijinja 渲染(见 `zhuye_prompt::template`)。
`FINAL ANSWER: <answer>` 收口模板是**格式契约**(测试与展示层剥标记依赖),
保留在常量 `zhuye_prompt::FINAL_ANSWER_TEMPLATE`,nudge 文案经变量注入。

## 按模型覆盖示例

只想给 Claude 换主 prompt 的最小文件(`.zhuye/prompts/claude.md`):

```markdown
## system.main
You are Claude, an expert pair programmer. ...(按 Anthropic 官方 agent
提示词风格调优)

## tool.bash
(可选)按 Claude 的工具使用习惯改写 bash 工具说明。
```

`claude/gpt/gemini/deepseek` 之外,任何 spec 的 model 段都可建同名精确
层文件,例如 `deepseek-chat.md`:

```markdown
## system.main
你是一名深度求索编码助手 ...(DeepSeek 特化措辞)
```

## 编程接口(facade / TUI / GUI)

```rust
use zhuye::{ZhuyeBuilder, PromptLibrary, PromptSet, init_library, library, split_spec};

// Builder 一行挂载自带提示词包(目录优先级最高,覆盖同名层):
let agent = ZhuyeBuilder::new("claude/claude-sonnet-4-5")
    .workspace("/path/to/ws")
    .with_prompts_dir("/path/to/bundled-prompts")
    .build()?;

// 或绕过 Builder 直接操作进程级库(default → 厂商 → 精确三级回退):
init_library(&[home_prompts_dir, ws_prompts_dir], None);
let set: PromptSet = library().resolve("anthropic/claude-sonnet-4-5");
let main_prompt = set.get_or("system.main", "fallback");
let (provider, model) = split_spec("anthropic/claude-sonnet-4-5");
```

Python 绑定(`zhuye-py`)同步暴露 `ZhuyeBuilder.prompts_dir(dir)`。

TUI/GUI 运行中选择提示词:提交 `Op::PromptCommand`(经
`thread.submit(...)`,与 `/plan` / `/goal` 同一 Op 通道),消费
`EventMsg::PromptList` / `EventMsg::PromptSwitched` 渲染选择 UI:

```rust
use zhuye::{Op, PromptAction, Submission};

// 列出可选层(弹层选择器用):
let sub = Submission::with_id("ui-1", Op::PromptCommand { action: PromptAction::List });
// 用户改了提示词文件后刷新(无参 = 全量重扫):
let sub = Submission::with_id("ui-2", Op::PromptCommand {
    action: PromptAction::Switch { layer: None },
});
```

## 运行时序

1. 入口(`zhuye-exec::bootstrap_m4` / `build_default_m4_with_prompts`)
   扫描 `<ws>/.zhuye/prompts` + `~/.zhuye/prompts` + Builder 附加目录,
   调 `init_library` 装配进程级 `PromptLibrary`(可重复调用整体重扫);
2. `pre_loop` 组装 system prompt:agent 定义来源为 `BuiltinDefault` 时按
   当前 spec 解析 `system.main`;用户自定义(`.zhuye/agents/*.md`)
   原文不动(`SystemPromptSource::Custom`);
3. 工具 description 覆盖发生在 **spec 组装处**(`apply_model_overrides`),
   不侵入 `Tool` trait —— MCP / LSP / plugin 外部工具实现者无感知;
4. nudge / active-mode / compact / 编排 prompt 在各自调用点按库解析,
   未配置模型层时与旧行为完全一致。

## 缓存安全性

同模型同提示词 → prompt cache 照常命中;热切换模型本身就会 cache break,
提示词随模型一起切换不会产生额外失效。
