# 实模型演练层(`tests/live/`)

这一层用**真实模型**跑烛夜的完整 agent 链路,补上 mock provider
永远测不到的行为:真实 SSE 帧、真实工具调用、真实限流(429)、真实
截断续作、真实思考过程。

## 与能力覆盖层的边界

| | 能力覆盖层 `tests/functional/` | 实模型层 `tests/live/` |
|---|---|---|
| 模型 | mock(`ZHUYE_MODEL=mock`) | 真实模型(清单接口实时拉取) |
| 断言 | 字段级、可逐字 | **协议形状级**,不断言回答文本 |
| 网络 | 全程离线 | 需要外网 + 免费额度 |
| 门禁 | **CI 强制**(`run_all.sh functional`) | 可选(`run_all.sh live`),默认 SKIP |

⚠️ **能力断言仍然只写在 `tests/functional/`** —— 本层只断言协议形状与
运行时行为,不重复能力覆盖。两层不重叠、不互相替代。

## 跑法

```bash
# 默认跳过(不发起任何网络请求,退出码 0)
./tests/run_all.sh live

# 真跑
ZHUYE_LIVE=1 ./tests/run_all.sh live

# 指定车道(默认 Kilo 免费池:免 Key、无客户端指纹门)
ZHUYE_LIVE=1 \
ZHUYE_LIVE_BASE_URL=https://api.kilo.ai/api/gateway \
ZHUYE_LIVE_API_KEY= \
./tests/run_all.sh live

# 指定模型(逗号分隔);不指定则从清单里取
ZHUYE_LIVE=1 ZHUYE_LIVE_MODELS=cohere/north-mini-code:free ./tests/run_all.sh live
```

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `ZHUYE_LIVE` | 空 | 设 `1` 才真跑(否则 SKIP) |
| `ZHUYE_LIVE_BASE_URL` | `https://api.kilo.ai/api/gateway` | provider 端点 |
| `ZHUYE_LIVE_API_KEY` | 空 | 免 Key 端点留空;dsh 网关填 `forwardKey` |
| `ZHUYE_LIVE_MODELS` | 空 | 逗号分隔模型 id;空则从清单取 |
| `ZHUYE_LIVE_MODEL_COUNT` | `3` | 从清单取几个 |
| `ZHUYE_LIVE_RETRIES` | `2` | 单个用例重试次数(限流是常态) |
| `ZHUYE_LIVE_RETRY_DELAY` | `5` | 重试间隔(秒) |
| `ZHUYE_LIVE_DRAIN_TIMEOUT` | `180` | 事件流等待预算(秒) |
| `ZHUYE_LIVE_TOKEN_BUDGET` | — | 预留:token 预算护栏 |

## 模型从哪来(不写死)

1. `ZHUYE_LIVE_MODELS` 给了 → 用它;
2. 否则调 `zhuye models list --free-only --json` → 拿 provider 的实时清单;
3. 逐个做**能力探测**(带 `tools` 的最小请求)→ 不支持 tools 的模型标为
   **跳过**而不是失败;
4. 取前 N 个(`ZHUYE_LIVE_MODEL_COUNT`)。

免费模型集合由上游决定,随时可能增删 —— **写死模型 id 必然腐烂**。

## 断言口径

只断言**协议形状**:

- 事件序:`turn_started` → delta → `token_count` → `turn_complete`;
- `turn_id` 首尾一致;
- `usage` 四字段齐全且 `total_tokens > 0`;
- 工具调用成对(`tool_call_begin` / `tool_call_end` 数量相等);
- `turn_complete.status == "success"`。

**不断言具体回答文本** —— 真实模型的输出不确定,逐字断言只会制造假失败。
内容类需求一律用宽松判定或直接跳过。

## 已知限制

- 免费池是**全局配额**,429 常见 → 用例自带重试;模型矩阵**必须串行**跑。
- 部分免费模型不支持 tools → 能力探测识别并跳过。
- 思考过程长度差异极大(有的模型思考 60~70s)→ 事件等待预算放宽到 180s。
- `timeout_secs` 必须放大(默认 60s 是 reqwest 的**总**请求时长,会把流式
  响应腰斩)→ 配置里固定 300。
