#!/usr/bin/env bash
# scripts/sdk_smoke.sh —— Python / TypeScript SDK 对真实 `reflect serve`
# 二进制的离线冒烟(mock provider,无网络、无 API key)。
#
# 覆盖(两个 SDK 各一遍):
#   1. spawn → session_configured 握手;
#   2. 多轮 prompt 保持上下文(mock 脚本逐行消耗,两轮回复不同文本);
#   3. 自定义工具注册 → LLM 调用 → SDK 本地执行 → 回执 → turn 完成。
#
# 前置:`cargo build --release -p reflect-cli`(target/release/reflect)。
# SDK 测试套件(sdks/*/tests)用 Node mock serve;本脚本补"真二进制"
# 这一环,两份覆盖互补。TS SDK 零运行时依赖,dist 产物可被 node 直接
# import,无需 npm install。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/reflect"
[ -x "$BIN" ] || { echo "FAIL: $BIN not built (cargo build --release -p reflect-cli)"; exit 1; }

TS_BIN="$ROOT/sdks/typescript"
[ -f "$TS_BIN/dist/index.js" ] || ( cd "$TS_BIN" && npm run build --silent )

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# mock 脚本 A(多轮):第 1 轮文本 "first-reply",第 2 轮 "second-reply"。
cat > "$TMP/multi.jsonl" <<'EOF'
{"type":"text","text":"first-reply"}
{"type":"text","text":"second-reply"}
EOF

# mock 脚本 B(远程工具):第 1 次调用发起 sdk_echo 工具,第 2 次回文本。
cat > "$TMP/tool.jsonl" <<'EOF'
{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}
{"type":"text","text":"tool-done"}
EOF

# ── Python SDK ─────────────────────────────────────────────────────────
REFLECT_BIN="$BIN" REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$TMP/multi.jsonl" \
PYTHONPATH="$ROOT/sdks/python" python3 - <<'PYEOF'
from reflect import ReflectAgent

agent = ReflectAgent.spawn()
# mock 脚本按模型调用顺序逐行消耗,两轮回复不同 → 验证会话状态保持。
first = agent.prompt("第一轮")
second = agent.prompt("第二轮")
assert first == "first-reply", f"first={first!r}"
assert second == "second-reply", f"second={second!r}"
agent.close()
print("[ok] python sdk: 两轮 prompt(上下文保持)")
PYEOF

REFLECT_BIN="$BIN" REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$TMP/tool.jsonl" \
PYTHONPATH="$ROOT/sdks/python" python3 - <<'PYEOF'
from reflect import ReflectAgent, ToolOutput, ContentBlock

calls = []

def handler(args):
    calls.append(args)
    return ToolOutput(
        content=[ContentBlock(type="text", text=f"echo:{args['q']}")],
        is_error=False,
        metadata={},
        elapsed_ms=0,
    )

agent = ReflectAgent.spawn()
agent.register_tool("sdk_echo", "echo back", {"type": "object"}, handler)
saw_complete = False
for ev in agent.submit("调工具"):
    if ev["msg"]["type"] == "turn_complete":
        saw_complete = True
        break
assert calls == [{"q": "ping"}], f"calls={calls!r}"
assert saw_complete, "未见 turn_complete"
agent.close()
print("[ok] python sdk: 自定义工具被 LLM 调用并在客户端执行")
PYEOF

# ── TypeScript SDK(dist 产物 + node 直跑)──────────────────────────────
cat > "$TMP/ts_driver.mjs" <<TSEOF
import { ReflectAgent } from '${TS_BIN}/dist/index.js';

const base = {
  bin: process.env.REFLECT_BIN,
  env: { REFLECT_MODEL: 'mock', REFLECT_MOCK_SCRIPT: process.env.SCRIPT },
};

if (process.env.SCENARIO === 'multi') {
  const agent = await ReflectAgent.spawn(base);
  const first = await agent.prompt('第一轮');
  const second = await agent.prompt('第二轮');
  if (first !== 'first-reply') throw new Error(\`first=\${first}\`);
  if (second !== 'second-reply') throw new Error(\`second=\${second}\`);
  await agent.close();
  console.log('[ok] typescript sdk: 两轮 prompt(上下文保持)');
}

if (process.env.SCENARIO === 'tool') {
  const calls = [];
  const agent = await ReflectAgent.spawn(base);
  await agent.registerTool('sdk_echo', 'echo back', { type: 'object' }, (args) => {
    calls.push(args);
    return {
      content: [{ type: 'text', text: \`echo:\${args.q}\` }],
      is_error: false,
      metadata: {},
      elapsed_ms: 0,
    };
  });
  let sawComplete = false;
  for await (const ev of await agent.submit('调工具')) {
    if (ev.msg.type === 'turn_complete') { sawComplete = true; break; }
  }
  if (calls.length !== 1 || calls[0].q !== 'ping') {
    throw new Error(\`calls=\${JSON.stringify(calls)}\`);
  }
  if (!sawComplete) throw new Error('未见 turn_complete');
  await agent.close();
  console.log('[ok] typescript sdk: 自定义工具被 LLM 调用并在客户端执行');
}
TSEOF

SCENARIO=multi SCRIPT="$TMP/multi.jsonl" REFLECT_BIN="$BIN" node "$TMP/ts_driver.mjs"
SCENARIO=tool SCRIPT="$TMP/tool.jsonl" REFLECT_BIN="$BIN" node "$TMP/ts_driver.mjs"

echo
echo "sdk smoke 全部通过"
