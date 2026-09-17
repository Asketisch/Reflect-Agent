# Reflect 示例插件:`demo`

五类 capability 各配一个最小可运行样例的演示插件:

| Capability | 文件 | 效果 |
|---|---|---|
| commands | `commands/hello.md` | `/demo:hello 张三` 展开为命令正文后提交 |
| agents | `agents/reviewer.md` | 模型可调用 `call_demo_reviewer` 子代理 |
| skills | `skills/demo-skill/SKILL.md` | 模型可 `load_skill` 激活的演示技能 |
| hooks | `hooks/hooks.json` | 每次工具调用后在 stderr 打一行日志 |
| mcp_servers | `.mcp.json` + `servers/echo_server.py` | `mcp__plugin:demo:echo__echo` 回显工具 |

## 安装与启用

```bash
# 1. 安装(复制到 ~/.reflect/plugins/cache 并登记)
reflect plugin install examples/plugin-demo

# 2. 启用(写回 config.toml [plugins].enabled_plugins;
#    id 必须是 name@marketplace 全名,本地安装为 demo@inline)
reflect plugin enable demo@inline

# 3. 验证
reflect plugin list
reflect exec "/demo:hello 张三"
```

也可以不装直接本地引用:把 `demo@inline` 加入 `~/.reflect/config.toml` 的
`[plugins].enabled_plugins` 后,用 `reflect plugin install` 指向本目录即可。

## MCP server 说明

`.mcp.json` 里的可执行入口使用 `${PLUGIN_ROOT}` 占位符 —— 安装时展开为
插件在 cache 中的绝对路径,因此插件移动 / 复制后仍然有效。echo server
只用 Python 标准库,无需 `pip install`。

## 安全模型

- 插件 hook 是外部进程(`ShellHook`):事件 JSON 走 stdin,决策 JSON 走
  stdout;超时 / 非零退出一律 fail-closed(Deny)。
- 插件注册的工具(agents / MCP)权限下限为 `Prompt`:即使声明 `Auto`,
  也会被抬升,调用前需要用户批准。
- `/demo:hello` 命令只是 prompt 模板替换,不注册任何工具、不改变模型
  可见面。
