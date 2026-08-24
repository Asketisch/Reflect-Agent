#!/usr/bin/env bash
# tests/run_all.sh —— 全量测试入口:Rust + Python SDK + TS SDK。
#
# 用法:
#   ./tests/run_all.sh            # 全部
#   ./tests/run_all.sh rust       # 只跑 Rust 侧
#   ./tests/run_all.sh sdk-python # 只跑 Python SDK
#   ./tests/run_all.sh sdk-ts     # 只跑 TS SDK
#
# 全程离线(REFLECT_MODEL=mock)+ 隔离 HOME。退出码 0 = 全部通过。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/lib.sh"

SUITES=("rust" "sdk-python" "sdk-ts")
if [ $# -gt 0 ]; then
    SUITES=("$@")
fi

cd "$ROOT"

# release 二进制与 examples 不存在则构建(幂等)
if [ ! -x "$REFLECT_BIN" ]; then
    echo "── 构建 release 二进制 ──"
    cargo build --release -p reflect-cli --quiet || exit 2
fi

OVERALL=0
for s in "${SUITES[@]}"; do
    case "$s" in
        rust)        RUNNER="$HERE/rust/run.sh" ;;
        sdk-python)  RUNNER="$HERE/sdk-python/run.sh" ;;
        sdk-ts)      RUNNER="$HERE/sdk-ts/run.sh" ;;
        *)
            echo "未知套件:$s(可选:rust / sdk-python / sdk-ts)" >&2
            exit 2
            ;;
    esac
    echo
    echo "╔══════════════════════════════════════════════════╗"
    echo "  套件:$s"
    echo "╚══════════════════════════════════════════════════╝"
    if bash "$RUNNER"; then
        echo "  ✅ $s 通过"
    else
        echo "  ❌ $s 失败"
        OVERALL=1
    fi
done

echo
if [ "$OVERALL" -eq 0 ]; then
    echo "══════════════ 全部测试套件通过 ══════════════"
else
    echo "══════════════ 存在失败套件(见上)══════════════"
fi
exit "$OVERALL"
