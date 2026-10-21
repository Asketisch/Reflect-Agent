#!/usr/bin/env bash
# tests/functional/run.sh —— 能力覆盖层入口(Python 驱动真二进制,原始 wire)。
#
# 这一层测的是 **Rust 代码在真产物上的行为**:起 `reflect serve` 子进程、
# 走原始 JSONL wire、断言事件字段 —— **不经过任何 SDK**(SDK 的 bug 会
# 把 Rust 的 bug 遮住)。
#
# 覆盖:基础通信 / 并发路由 / 工具调用 / 审批交互 / 会话状态 op /
#      重连恢复 / 容错 / 持久化 / 编排 / CLI 能力面。
#
# 全程离线(REFLECT_MODEL=mock)+ 隔离 HOME(mktemp),不污染真实 ~/.reflect。
# 退出码 0 = 全部通过;缺 pytest 时 SKIP(不算失败)。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── 能力覆盖层:Python 驱动真二进制(原始 wire)──"
if require_cmd python3 && python3 -m pytest --version >/dev/null 2>&1; then
    if HOME="$(mktemp -d)" REFLECT_BIN="$REFLECT_BIN" \
        python3 -m pytest "$HERE" -v --tb=short; then
        echo "  [ok]   能力覆盖层全绿"
    else
        echo "  [FAIL] 能力覆盖层存在失败"
        RC=1
    fi
else
    echo "  [SKIP] 未安装 python3/pytest,跳过"
fi

exit "$RC"
