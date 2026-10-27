#!/usr/bin/env bash
# tests/sdk-python/run.sh —— Python SDK 绑定层测试入口。
#
# 两批用例:
#   1. tests/sdk-python/test_sdk_real_bin.py —— 真二进制 + mock provider,
#      断言 **Python SDK 客户端自身的行为**(握手/多轮 prompt/远程工具本地
#      执行/非 turn op 收尾/interrupt/close 幂等/坏二进制/handler 异常回执);
#   2. sdks/python/tests/test_protocol.py     —— Node mock serve 协议单测
#      (仓库原有,随 SDK 包发布,一并纳入统一入口)。
#
# 服务端语义(19 Op / 事件流 / 重连 / 容错)由能力覆盖层承担:
#   tests/functional/(原始 wire,不经 SDK)。
#
# 退出码 0 = 全部通过;缺 pytest / node 时相应批次 SKIP(不算失败)。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── Python SDK 绑定层:真二进制端到端(pytest)──"
if require_cmd python3 && python3 -m pytest --version >/dev/null 2>&1; then
    if HOME="$(mktemp -d)" ZHUYE_BIN="$ZHUYE_BIN" \
        PYTHONPATH="$ROOT/sdks/python" \
        python3 -m pytest "$HERE/test_sdk_real_bin.py" -v --tb=short; then
        echo "  [ok]   SDK 绑定层全绿"
    else
        echo "  [FAIL] SDK 绑定层存在失败"
        RC=1
    fi
else
    echo "  [SKIP] 未安装 python3/pytest,跳过"
fi

echo
echo "── Python SDK:协议单测(Node mock serve)──"
if require_cmd python3 node && python3 -m pytest --version >/dev/null 2>&1; then
    if HOME="$(mktemp -d)" ZHUYE_BIN="$ZHUYE_BIN" \
        PYTHONPATH="$ROOT/sdks/python" \
        python3 -m pytest "$ROOT/sdks/python/tests" -v --tb=short; then
        echo "  [ok]   协议单测全绿"
    else
        echo "  [FAIL] 协议单测存在失败"
        RC=1
    fi
else
    echo "  [SKIP] 缺 python3/node/pytest,跳过"
fi

exit "$RC"
