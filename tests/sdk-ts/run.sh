#!/usr/bin/env bash
# tests/sdk-ts/run.sh —— TypeScript SDK 测试入口。
#
# 两批用例:
#   1. tests/sdk-ts/real-bin.test.mjs     —— 真二进制 + mock provider
#      (核心协议回路:握手/多轮/自定义工具/interrupt/close 幂等);
#      tests/sdk-ts/ops.test.mjs          —— 深度 Op 面
#      (非 turn ops 全量收尾 / 权限模式持久化 / MCP 审批全链路 / 孤儿回执);
#   2. sdks/typescript/tests/*.test.ts    —— mock serve 协议单测
#      (vitest,仓库原有,一并纳入统一入口)。
#
# 退出码 0 = 全部通过;缺 node / vitest 依赖时相应批次 SKIP。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── TS SDK:真二进制端到端 + 深度 Op 面(node --test)──"
if require_cmd node; then
    # dist 产物缺失、或比任一 src 源文件旧(改过 protocol.ts/agent.ts)
    # 则先构建 —— 测试 import 的是 dist,陈旧产物会让新协议变体/类型
    # 丢失,用例挂死或类型不符。需 typescript devDependency(已随仓库装好)。
    if [ ! -f "$ROOT/sdks/typescript/dist/index.js" ] || \
       [ -n "$(find "$ROOT/sdks/typescript/src" -name '*.ts' -newer "$ROOT/sdks/typescript/dist/index.js" -print -quit 2>/dev/null)" ]; then
        (cd "$ROOT/sdks/typescript" && npm run build --silent) || {
            echo "  [FAIL] dist 构建失败"
            exit 1
        }
    fi
    if HOME="$(mktemp -d)" REFLECT_BIN="$REFLECT_BIN" \
        node --test "$HERE/real-bin.test.mjs" "$HERE/ops.test.mjs"; then
        echo "  [ok]   真二进制端到端 + 深度 Op 全绿"
    else
        echo "  [FAIL] 真二进制端到端 / 深度 Op 存在失败"
        RC=1
    fi
else
    echo "  [SKIP] 未安装 node,跳过"
fi

echo
echo "── TS SDK:协议单测(vitest,mock serve)──"
if [ -d "$ROOT/sdks/typescript/node_modules/vitest" ]; then
    if (cd "$ROOT/sdks/typescript" && npx vitest run); then
        echo "  [ok]   协议单测全绿"
    else
        echo "  [FAIL] 协议单测存在失败"
        RC=1
    fi
else
    echo "  [SKIP] sdks/typescript 未安装 vitest 依赖,跳过"
fi

exit "$RC"
