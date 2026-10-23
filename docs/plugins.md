# Reflect 插件开发指南

Reflect 的插件系统是**声明式 / 数据驱动**的:插件是磁盘上一组遵循目录约定
的文件(`plugin.toml` manifest + 五类 capability 资源),由运行时扫描后挂载
到共享 registry。没有动态库加载,插件不携带原生代码 —— 跨进程逻辑通过
shell hook 与 MCP server 表达,权限模型与内置工具一致。

> 兼容说明:manifest 同时支持 `plugin.toml`(主格式)与
> `.claude-plugin/plugin.json`(Claude Code 兼容格式),五类 capability
> 语义对齐 Claude Code 插件,便于现成插件迁移。

## 快速开始

```bash
# 安装(复制到 ~/.reflect/plugins/cache 并登记 installed_plugins.json)
reflect plugin install examples/plugin-demo

# 启用(写回 ~/.reflect/config.toml 的 [plugins].enabled_plugins;
# id 必须是 name@marketplace 全名,本地安装为 demo@inline)
reflect plugin enable demo@inline

# 使用:slash 命令展开
reflect exec "/demo:hello 张三"
```

可运行的完整示例见 [`examples/plugin-demo/`](../examples/plugin-demo/)。

## 插件目录结构

```
my-plugin/
├── plugin.toml              # 必需:manifest
├── commands/*.md            # slash 命令(prompt 模板)
├── agents/*.md              # 子代理定义
├── skills/<name>/SKILL.md   # 技能
├── hooks/hooks.json         # 外部进程 hook(Claude Code 风格)
└── .mcp.json                # MCP server 声明
```

`plugin.toml` 最小示例(全部字段见 `PluginManifest`):

```toml
name = "my-plugin"          # 必填,kebab-case
version = "0.1.0"
description = "..."
commands    = "./commands"  # 五类 capability 均可省略
agents      = "./agents"
skills      = "./skills"
hooks       = "./hooks/hooks.json"
mcp_servers = "./.mcp.json"
```

## 五类 capability

### commands —— slash 命令(prompt 模板)

`commands/` 下的 `*.md` 文件即命令,文件名(去 `.md`)是命令 basename,
子目录作为命名空间,全名为 `<插件名>:<命名空间>:<basename>`
(如 `demo:hello`、`demo:utils:lint`)。

frontmatter 支持 `description`。正文是 prompt 模板,支持参数占位符:

| 占位符 | 展开为 |
|---|---|
| `$ARGUMENTS` | 命令名之后的全部参数原文 |
| `$1` .. `$9` | 按空白切分的位置参数(缺失替换为空串) |

展开规则:

- 用户输入 `/demo:hello 张三 你好` → 查 `demo:hello` 命令 → 读 md、
  剥 frontmatter、替换占位符 → 用展开文本替换原输入提交;
- 正文没有任何占位符且参数非空时,参数以空行追加在正文末尾;
- 未命中任何命令的 `/xxx` 输入原样透传,不影响普通 prompt;
- 命令只是 **prompt 模板替换**:不注册工具、不改变模型可见面。
  展开后的 Submission 在 `source_command` 字段标注来源命令,
  随 rollout 持久化。

### agents —— 子代理

`agents/*.md` 的 frontmatter 解析为 `SubAgentSpec` 并注册
`call_<role>` 工具(模型可委派):

```markdown
---
name: My Reviewer
description: 你是一名代码评审子代理……(当前版本 system prompt 取自 description)
tools: [read, grep]
---
```

`role` 由插件全名派生(`:` 替换为 `_`,截断 32 字符)。工具权限下限为
`Prompt`(见安全模型)。

### skills —— 技能

`skills/<name>/SKILL.md`,frontmatter 至少包含 `description`。挂载后进入
`SkillsCatalog`,模型可经内置 `load_skill` 工具按需激活(渐进披露)。

### hooks —— 外部进程 hook

`hooks.json` 采用 Claude Code 风格:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "echo done >&2", "timeout": 10 }
        ]
      }
    ]
  }
}
```

支持的事件:`PreToolUse` / `PostToolUse` / `PostToolUseFailure` / `Stop` /
`SessionStart` / `UserPromptSubmit` / `PreCompact` / `TaskCreated` /
`TaskCompleted` / `TaskUpdated`。每条声明包装为 `ShellHook`:事件 JSON 写入
子进程 stdin,子进程用 stdout 返回决策 JSON(`allow` / `deny` / `ask`);
**超时或非零退出一律 fail-closed(Deny)**。

### mcp_servers —— MCP server

`.mcp.json` 为扁平 map(`{"<server名>": {"command": ..., "args": [...]}}`,
`command` 为 stdio 传输、`url` 为 streamable-http 传输)。挂载后工具以
`mcp__plugin:<插件名>:<server名>__<工具名>` 全名注入 `ToolRegistry`,
对模型可见;插件禁用时 server 停止、工具反注册。

## `${PLUGIN_ROOT}` 占位符

插件安装后会被复制到 `~/.reflect/plugins/cache/<marketplace>/<插件>/<版本>/`,
manifest 里的相对路径不再指向原目录。hook 命令与 MCP server 的
`command` / `args` / `env` 值中的 `${PLUGIN_ROOT}` 会在挂载时展开为插件
安装目录的绝对路径:

```json
{
  "echo": {
    "command": "python3",
    "args": ["${PLUGIN_ROOT}/servers/echo_server.py"]
  }
}
```

## 生命周期与热重载

- **安装**:`reflect plugin install <目录|git-url|tar>` → 复制进 cache 并
  登记;支持 marketplace(Git / GitHub / URL / 文件 / 目录五种源)与
  依赖闭包安装(`reflect plugin marketplace add ...`)。
- **启用 / 禁用**:权威表是 `~/.reflect/config.toml` 的
  `[plugins].enabled_plugins`(条目为 `name@marketplace` 全名);
  `reflect plugin enable demo@inline` 直接改写该列表。
- **挂载**:进程启动时按 enabled 列表挂载;运行中改动 config 的
  `[plugins]` 段会 diff 同步(新启用的挂载、移除的反注册),无需重启。
- **作用域**:`installed_plugins.json` 支持 Managed / User / Project /
  Local 四级 scope,同 id 多 scope 共存时按优先级取用。

## 宿主支持矩阵

| 宿主 | 插件挂载 | slash 命令展开 |
|---|---|---|
| `reflect exec`(普通) | ✅ 启动挂载 + 热重载 | ✅ `/plugin:ns:name args` |
| `reflect exec --resume` / `-c` | ✅(与普通路径同权) | —(resume prompt 为合成文本) |
| `reflect serve`(SDK 入口) | ✅(与 exec 共享装配) | ✅(单条 Text 的 UserInput;展开失败原样转发) |
| Rust 门面 `ReflectBuilder::build_async` | ✅(`with_plugins(None)` 按 config) | 调用方用 `Reflect::plugin_runtime()` + `expand_user_input` 自行展开 |
| Rust 门面 `ReflectBuilder::build`(同步) | ❌(保持最小语义) | ❌ |
| Python `reflect_py.ReflectBuilder(...).plugins().build()` | ✅(内部走 build_async) | 调用方自行展开 |

## 安全模型

- **外部工具权限下限**:插件注册的所有工具(agents / MCP)即使声明
  `Auto` 也会被 `FloorEnforcingTool` 抬升到 `Prompt` —— 调用前需用户批准。
- **hook fail-closed**:插件 hook 是任意可执行命令,超时 / 崩溃 / 非零
  退出都按 Deny 处理,不会因插件故障放行危险操作。
- **MCP 进程隔离**:stdio MCP server 以干净 env 启动(只透传
  HOME / PATH 与用户显式声明的 env),不继承宿主密钥。
- **commands 无副作用**:命令展开是纯文本替换,发生在提交前、由宿主
  而非模型触发。
- 路径安全:capability 路径不允许逃出插件目录;`PluginId` 严格
  kebab-case 校验,`inline` / `builtin` 为保留 marketplace 名。

## API 索引(`reflect-plugin` crate)

- 数据模型:`PluginManifest` / `PluginId` / `PluginStatus` / `InstalledPluginsFile`
- 管理:`PluginManager`(install / uninstall / list)、`DepResolver`、marketplace fetcher
- 挂载:`loader::{LoaderRegistries, register, unregister}`、
  `CommandRegistry`、`expand_user_input`
- 运行时装配:`runtime::{PluginRuntime, bootstrap_plugins, reload_plugins}`
  (exec / serve / 门面 / Python 四条路径共用)
