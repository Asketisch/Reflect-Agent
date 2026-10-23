# 免费模型实践测试指南

用**免费模型池**跑 Reflect 的完整 agent 链路 —— 不需要任何 API Key,
也不产生费用。

本文覆盖两条车道、模型发现机制、配置模板、已知限制与排障。

## 目录

- [为什么需要它](#为什么需要它)
- [两条车道](#两条车道)
- [模型发现:从接口获取,不写死](#模型发现从接口获取不写死)
- [配置模板](#配置模板)
- [跑起来](#跑起来)
- [实模型演练层](#实模型演练层)
- [横向对比演练](#横向对比演练)
- [已知限制](#已知限制)
- [排障](#排障)

## 为什么需要它

能力覆盖层(`tests/functional/`)用 mock provider —— 确定性、离线、
是 CI 门禁。但 mock 永远测不到:真实 SSE 帧的形状、真实模型的工具调用
质量、真实的 429 限流、真实思考过程、真实的截断续作。

免费模型池让这些**零成本**地跑起来。

## 两条车道

| 车道 | 端点 | 凭据 | 前置条件 |
|---|---|---|---|
| **Kilo 免费池**(默认) | `https://api.kilo.ai/api/gateway` | 免 Key | 无 |
| **OpenCode inference** | `https://opencode.ai/inference/v1` | `Bearer public` | 必须经 dsh 网关 |

Kilo 车道**零外部依赖** —— 只改 `config.toml` 就能用。

OpenCode 车道有 **v2 客户端指纹门**:UA 必须是 `opencode/latest/2.0.26/cli`、
四个会话头、铸造形状的 session id、**逐字**的 `read` / `shell` 工具定义、
且绝不能下发 `tool_choice: none`。这些由 dsh-our-free-model 的本地网关
补齐(它还会把与官方定义不同的同名工具改名并在响应侧还原 —— Reflect 自带
一个名为 `read` 的内置工具,走这条车道时会被改名再还原)。

```bash
# 启停网关(不自动 npm install —— 装依赖会改宿主状态,需显式触发)
cd ../dsh-our-free-model && npm install
scripts/ofm_gateway.sh start
scripts/ofm_gateway.sh status
```

## 模型发现:从接口获取,不写死

免费模型集合由**上游**决定,随时可能增删 —— 写死模型 id 必然腐烂。
Reflect 因此内置了**模型发现能力**:

```bash
reflect models list --free-only          # 人看
reflect models list --free-only --json   # 脚本消费
reflect models pick --free-only          # 只打印一个 model id
```

「免费」的判定走**规则**(`reflect_llm::FreeModelRules`),三通道:

1. **provider 显式标记** —— Kilo 的 `isFree: true`(权威);
2. **白名单** —— 不带 free 标记但确实免费的 id(默认三个,可配置);
3. **id 模式** —— 默认 `["-free", ":free", "/free"]`。

规则**不包含模型清单** —— 模型本身永远来自 provider 的清单端点。

配置里可以直接写 `model = "free"`:运行时查清单 → 按规则挑一个 →
写进运行时配置。事件流里看到的是**真实 model id**,不是占位符。

```toml
[model_discovery]
enabled = true                    # 关掉则 model = "free" 直接报错
timeout_secs = 20
prefer = "code"                   # id 子串偏好(可空)
always_free = ["union-alpha", "big-pickle", "space-bunny-free"]
free_id_patterns = ["-free", ":free", "/free"]
```

## 配置模板

### Kilo 车道(默认,免 Key)

```toml
# ~/.reflect/config.toml
[hooks]
enabled = []                      # 演练时关掉内置 hook(只验引擎行为)

[active]
provider = "openai"

[openai]
api_key = ""                      # 免 Key —— 空 key 不发 Authorization 头
base_url = "https://api.kilo.ai/api/gateway"
timeout_secs = 300                # ⚠️ 见下方「已知限制」
model = "free"                    # 运行时从清单里挑

[[openai.credentials]]
label = "kilo-free"
api_key = ""
base_url = "https://api.kilo.ai/api/gateway"
model = "free"

[routing]
max_attempts = 8
[routing.main]
primary = "openai/free"
fallbacks = ["openai/cohere/north-mini-code:free"]
```

### dsh 网关车道(OpenCode 车道)

```toml
[openai]
api_key = "<forwardKey>"           # ~/.our-free-model/settings.json
base_url = "http://127.0.0.1:18900"
timeout_secs = 300
model = "free"
```

### 跨模型降级链

`[routing].fallbacks` 是**跨模型**降级链:当前模型的凭据池全不可用
(全冷却 / 全被排除)时,自动切到链上的下一个模型继续本轮。

```toml
[routing.main]
primary = "openai/cohere/north-mini-code:free"
fallbacks = [
  "openai/nvidia/nemotron-3.5-lightning:free",
  "openai/minimax/minimax-m3.1-flash:free",
]
```

⚠️ `fallbacks` 里写 `"openai/free"` **不会**自动挑不同模型 —— `free`
每次都解析成同一个。真正降级要写具体 id(或用脚本按清单生成)。

切换时引擎会发 `routing(kind="spec_fallback")` 事件,`from_credential` /
`to_credential` 装的是 `"provider/model"` 形式的 spec。

## 跑起来

```bash
# 单轮
reflect exec "用一句话回答:1+1 等于几?"

# 看当前可用模型
reflect models list --free-only

# 自检(含模型清单检查)
reflect doctor --check-network
```

## 实模型演练层

```bash
REFLECT_LIVE=1 ./tests/run_all.sh live
```

细节见 [`tests/live/README.md`](../tests/live/README.md)。要点:

- 默认 SKIP(不发起任何网络请求),不进 CI 门禁;
- 模型从接口实时获取 + 能力探测(不支持 tools 的模型**跳过**而非失败);
- 断言只到**协议形状**(事件序 / 终态 / usage 四字段 / 工具成对),
  **不断言回答文本**;
- 上游限流(429)判为**跳过**而不是失败 —— 测的是代码,不是配额。

## 横向对比演练

```bash
scripts/free_model_practice.sh --count 3
scripts/free_model_practice.sh --models cohere/north-mini-code:free --out report.md
```

输出 Markdown 对比表(终态 / 耗时 / token / 工具调用数 / 错误分类)。
**不是门禁** —— 退出码只反映脚本自身错误。

## 已知限制

| 限制 | 说明 | 缓解 |
|---|---|---|
| **配额是全局的** | 免费池 429 常见(OpenCode 车道按会话计额度,`retry-after` 可达 12h) | 用例自带重试;429 判为跳过;**矩阵必须串行** |
| **地区门 403** | 免费车道按出口 IP 判地区 | 换出口后重跑 |
| **模型集合随时变** | 上游随时下架某个 id | 清单每次实时拉取;fallback 链兜底 |
| **慢首字 / 思考静默** | 实测首字 >30s、思考阶段可静默 60~70s | `timeout_secs` **必须放大到 300** —— reqwest 的 timeout 是「建连→body 读完」的**总**时长,默认 60s 会把流式响应腰斩 |
| **思考过程字段名不一** | 多数端点用 `delta.reasoning_content`,免费池用 `delta.reasoning` | Reflect 两个都认(`reasoning_content` 优先) |
| **usage 可能在结束原因之后** | 部分端点把 usage 放在尾部额外 chunk | Reflect 收到结束原因后会在有界窗口内继续读 usage |
| **部分模型不支持 tools** | provider 直接 400 | 能力探测识别并跳过 |
| **prompt 可能被上游记录** | Kilo 模型卡明示 | **只用合成数据**,不要喂真实仓库内容 / 凭据 |
| **`context_window` 可能不披露** | OpenCode 原生车道只给 id | 用 `[context_windows]` 段显式声明 |

## 排障

**`no model configured for the active provider`**
→ 配置里没写 model。写具体 id,或写 `model = "free"`(需 `[model_discovery]
enabled = true`)。

**`cannot fetch model list` / `provider 不支持模型清单`**
→ 该 provider 没有清单端点。显式配置具体 model id。

**`PAID_MODEL_AUTH_REQUIRED`(401)**
→ 请求打到了**付费**模型上。检查 model id 拼写;确认 `base_url` 指向
免费池。

**`ALL_CREDENTIALS_EXHAUSTED`**
→ 池内凭据全不可用(常见于 429 全冷却)。配多个 credential,或用
`[routing].fallbacks` 做跨模型降级。

**思考过程看不到**
→ 端点用了第三种字段名(`delta.reasoning_details[]`),当前不解析 ——
见「已知限制」。

**流式响应被腰斩**
→ `timeout_secs` 太小。放大到 300。
