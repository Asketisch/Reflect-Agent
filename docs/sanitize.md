# 输出脱敏(Sanitize)

> 实现见 `reflect-tools/src/sanitize/mod.rs` 与
> `reflect-sanitize`(独立 pattern 库);配置 schema 见
> `reflect-config/src/schema.rs::SanitizeSection`。
> `/hooks ls` 与本文档展示注册的 pattern 清单。

## 1. 目的

工具输出 / rollout 持久化 / 遥测三处统一走 `Sanitizer`:在**离开进程
边界之前**把密钥类内容替换为 `[REDACTED:<id>]` 占位,防止 API key、
私钥等随 JSONL 落盘或进入 LLM 上下文。

## 2. 默认 pattern(10 类)

| id | 命中形态 | 替换 |
|---|---|---|
| `AWS_ACCESS_KEY` | `AKIA…` / `ASIA…`(16 位) | `[REDACTED:aws_key]` |
| `ANTHROPIC_KEY` | `sk-ant-…`(≥20) | `[REDACTED:anthropic_key]` |
| `OPENAI_KEY` | `sk-…`(≥20) | `[REDACTED:openai_key]` |
| `GITHUB_TOKEN` | `ghp_` / `gho_` / `ghs_` / `ghr_` / `ghu_` 前缀(≥30) | `[REDACTED:github_token]` |
| `PRIVATE_KEY_BLOCK` | `-----BEGIN … PRIVATE KEY-----` 整块 | `[REDACTED:private_key]` |
| `JWT` | `eyJ….eyJ….…` 三段 | `[REDACTED:jwt]` |
| `DB_URL` | `postgres://` / `mysql://` / `mongodb://` / `redis://` / `amqp(s)://` 连接串(含凭据部分) | scheme 保留 + `[REDACTED:db_url]` |
| `SLACK_TOKEN` | `xox[abprs]-…` | `[REDACTED:slack_token]` |
| `BEARER_TOKEN` | `Bearer …`(大小写不敏感) | `Bearer [REDACTED:bearer]` |
| `KEY_ASSIGN` | `(key|token|secret|password|credential|passwd|pwd) = value` 赋值 | 键保留 + `[REDACTED:key_assign]` |

顺序敏感:具体 provider key 先于 `KEY_ASSIGN`;`ANTHROPIC_KEY` 先于
`OPENAI_KEY`(`sk-ant-…` 是 `sk-…` 的子集)。`KEY_ASSIGN` 的值字符类
排除 `[` `]`,保证 `[REDACTED]` 不会被二次匹配(Rust regex crate 无
look-around,用显式前缀捕获组代替)。

## 3. 配置(config.toml `[sanitize]`)

```toml
[sanitize]
enabled = true                          # 总开关
marker = "[HIDDEN]"                     # 自定义替换占位符(默认 [REDACTED:<id>])
disable_default_patterns = false        # 关闭默认 10 类(高级用户)
extra_patterns = [                      # 追加自定义 regex(命中即整段替换)
  "(?i)\\bmy_token\\s*=\\s*\\S+",
]
```

regex 编译失败启动即报错(fail-fast,不静默跳过)。

## 4. 应用点

- 工具输出进上下文前(`ToolOutput` 序列化路径);
- rollout JSONL 写盘前(单条超 16 KiB 先截断再脱敏);
- 遥测 / LLM trace(`~/.reflect/traces/model-io/`)写盘前。

密钥**来源**侧的防护是另一层:MCP 子进程 `env_clear()` 只传白名单
env;credential 支持 env 引用而非明文落 config。两层叠加 —— 来源
不泄漏 + 出口兜底。
