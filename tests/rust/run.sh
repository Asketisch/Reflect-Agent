#!/usr/bin/env bash
# tests/rust/run.sh —— Rust 侧测试总入口。
#
# 依次执行:
#   1. workspace_tests.sh   cargo test / clippy / fmt / doc
#   2. cli_core.sh          核心 CLI 端到端(exec/serve/session/traces/login/config)
#   3. cli_modules.sh       模块 CLI 端到端(mcp/lsp/plugin/task/pipeline/discussion/...)
#   4. examples_e2e.sh      reflect 库 examples 冒烟
#   5. cli_deep.sh          全 17 子命令深度使用测试(参数矩阵/退出码/输出契约)
#   6. serve_ops.sh         serve 协议 19 Op 全量 + 事件断言
#   7. tools_drive.sh       内置工具全量驱动(mock LLM 逐工具调一遍)
#   8. integrations.sh      MCP/skill/notes/bubble/热重载/plugin 集成链路
#
# 全程离线(REFLECT_MODEL=mock)+ 隔离 HOME,不污染真实 ~/.reflect。
# 退出码 0 = 全部通过。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

cd "$ROOT"

# release 二进制不存在则构建(幂等)
if [ ! -x "$REFLECT_BIN" ]; then
    echo "── 构建 release 二进制 ──"
    cargo build --release -p reflect-cli --quiet || exit 2
fi

TOTAL_RC=0

echo
echo "════════════════════════════════════════════════"
echo "  1/8 workspace 测试(cargo test + lint + doc)"
echo "════════════════════════════════════════════════"
bash "$HERE/workspace_tests.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  2/8 核心 CLI 端到端"
echo "════════════════════════════════════════════════"
bash "$HERE/cli_core.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  3/8 模块 CLI 端到端"
echo "════════════════════════════════════════════════"
bash "$HERE/cli_modules.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  4/8 examples 冒烟"
echo "════════════════════════════════════════════════"
bash "$HERE/examples_e2e.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  5/8 CLI 全子命令深度使用测试"
echo "════════════════════════════════════════════════"
bash "$HERE/cli_deep.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  6/8 serve 协议 19 Op 全量"
echo "════════════════════════════════════════════════"
bash "$HERE/serve_ops.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  7/8 内置工具全量驱动"
echo "════════════════════════════════════════════════"
bash "$HERE/tools_drive.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  8/8 集成链路(MCP/skill/notes/bubble/热重载/plugin)"
echo "════════════════════════════════════════════════"
bash "$HERE/integrations.sh" || TOTAL_RC=1

echo
if [ "$TOTAL_RC" -eq 0 ]; then
    echo "  ✅ Rust 测试套件全部通过"
else
    echo "  ❌ Rust 测试套件存在失败(见上方 [FAIL] 行)"
fi
exit "$TOTAL_RC"
