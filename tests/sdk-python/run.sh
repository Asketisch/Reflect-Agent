#!/usr/bin/env bash
# tests/sdk-python/run.sh —— Python SDK 测试入口。
#
# 两批用例:
#   1. tests/sdk-python/test_sdk_real_bin.py  —— 真二进制 + mock provider
#      (核心协议回路:握手/多轮/自定义工具/interrupt/close/坏二进制);
#      tests/sdk-python/test_sdk_ops.py       —— 深度 Op 面
#      (非 turn ops 全量收尾 / 权限模式持久化 / MCP 审批全链路 / 孤儿回执);
#   2. sdks/python/tests/test_protocol.py     —— Node mock serve 协议单测
#      (仓库原有,一并纳入统一入口)。
#
# 退出码 0 = 全部通过;缺 pytest / node 时相应批次 SKIP(不算失败)。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── Python SDK:真二进制端到端 + 深度 Op 面(pytest)──"
if require_cmd python3 && python3 -m pytest --version >/dev/null 2>&1; then
    if HOME="$(mktemp -d)" REFLECT_BIN="$REFLECT_BIN" \
        PYTHONPATH="$ROOT/sdks/python" \
        python3 -m pytest "$HERE/test_sdk_real_bin.py" "$HERE/test_sdk_ops.py" -v --tb=short; then
        echo "  [ok]   真二进制端到端 + 深度 Op 全绿"
    else
        echo "  [FAIL] 真二进制端到端 / 深度 Op 存在失败"
        RC=1
    fi
else
    echo "  [SKIP] 未安装 python3/pytest,跳过"
fi

echo
echo "── Python SDK:协议单测(Node mock serve)──"
if require_cmd python3 node && python3 -m pytest --version >/dev/null 2>&1; then
    if HOME="$(mktemp -d)" REFLECT_BIN="$REFLECT_BIN" \
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
