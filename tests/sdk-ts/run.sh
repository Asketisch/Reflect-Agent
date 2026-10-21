#!/usr/bin/env bash
# tests/sdk-ts/run.sh —— TypeScript SDK 绑定层测试入口。
#
# 两批用例:
#   1. tests/sdk-ts/real-bin.test.mjs     —— 真二进制 + mock provider,
#      断言 **TS SDK 客户端自身的行为**(握手/单轮 prompt/远程工具本地执行/
#      非 turn op 收尾/close 幂等);
#   2. sdks/typescript/tests/*.test.ts    —— mock serve 协议单测
#      (vitest,仓库原有,一并纳入统一入口)。
#
# 服务端语义(19 Op / 事件流 / 重连 / 容错)由能力覆盖层承担:
#   tests/functional/(原始 wire,不经 SDK)。
#
# 退出码 0 = 全部通过;缺 node / vitest 依赖时相应批次 SKIP。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── TS SDK 绑定层:真二进制端到端(node --test)──"
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
        node --test "$HERE/real-bin.test.mjs"; then
        echo "  [ok]   SDK 绑定层全绿"
    else
        echo "  [FAIL] SDK 绑定层存在失败"
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
