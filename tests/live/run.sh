#!/usr/bin/env bash
# tests/live/run.sh —— 实模型演练层入口(真实模型跑真二进制)。
#
# 这一层与能力覆盖层(`tests/functional/`)的区别:
#   - 能力覆盖层:mock provider + 确定性断言,是 CI 门禁;
#   - 实模型层:真实模型 + 协议形状级断言,是**人工/CI 夜间跑的补充门禁**。
#
# 模型**从 provider 接口实时获取**(见 conftest.py),不写死模型 id ——
# 免费模型集合由上游决定,随时可能增删。
#
# 默认 SKIP:不设 `REFLECT_LIVE=1` 时不发起任何网络请求,退出码 0。
#
# 用法:
#   REFLECT_LIVE=1 ./tests/run_all.sh live
#   REFLECT_LIVE=1 REFLECT_LIVE_MODELS=a,b ./tests/run_all.sh live

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── 实模型演练层:真实模型驱动真二进制(原始 wire)──"

if [ "${REFLECT_LIVE:-}" = "" ]; then
    echo "  [SKIP] 未设 REFLECT_LIVE=1,跳过(不发起任何网络请求)"
    exit 0
fi

if ! require_cmd python3 || ! python3 -m pytest --version >/dev/null 2>&1; then
    echo "  [SKIP] 未安装 python3/pytest,跳过"
    exit 0
fi

echo "  车道: ${REFLECT_LIVE_BASE_URL:-https://api.kilo.ai/api/gateway}"
echo "  模型来源:provider 清单接口(实时拉取)"

if HOME="$(mktemp -d)" REFLECT_BIN="$REFLECT_BIN" \
    python3 -m pytest "$HERE" -v --tb=short -p no:cacheprovider; then
    echo "  [ok]   实模型演练层全绿"
else
    echo "  [FAIL] 实模型演练层存在失败"
    RC=1
fi

exit "$RC"
