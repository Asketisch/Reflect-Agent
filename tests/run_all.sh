#!/usr/bin/env bash
# tests/run_all.sh —— 全量测试入口:能力覆盖层 + Rust 侧 + Python SDK + TS SDK。
#
# 用法:
#   ./tests/run_all.sh            # 全部
#   ./tests/run_all.sh functional # 功能覆盖(能力覆盖层 + Rust 参数面 + 双 SDK 绑定层)
#   ./tests/run_all.sh functional-layer  # 只跑能力覆盖层(Python 打真二进制)
#   ./tests/run_all.sh rust       # 只跑 Rust 侧
#   ./tests/run_all.sh sdk-python # 只跑 Python SDK 绑定层
#   ./tests/run_all.sh sdk-ts     # 只跑 TS SDK 绑定层
#
# `functional` 是 CI 的门禁入口:跳过 cargo test / clippy / fmt / doc
# (那部分由既有 CI job 覆盖),跑真正的功能覆盖。
#
# 全程离线(REFLECT_MODEL=mock)+ 隔离 HOME。退出码 0 = 全部通过。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/lib.sh"

SUITES=("functional" "rust" "sdk-python" "sdk-ts")
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
        functional)  RUNNERS=("$HERE/functional/run.sh" "$HERE/rust/run.sh" "$HERE/sdk-python/run.sh" "$HERE/sdk-ts/run.sh")
                     export REFLECT_SKIP_WORKSPACE=1 ;;
        functional-layer) RUNNERS=("$HERE/functional/run.sh") ;;
        rust)        RUNNERS=("$HERE/rust/run.sh") ;;
        sdk-python)  RUNNERS=("$HERE/sdk-python/run.sh") ;;
        sdk-ts)      RUNNERS=("$HERE/sdk-ts/run.sh") ;;
        *)
            echo "未知套件:$s(可选:functional / functional-layer / rust / sdk-python / sdk-ts)" >&2
            exit 2
            ;;
    esac
    echo
    echo "╔══════════════════════════════════════════════════╗"
    echo "  套件:$s"
    echo "╚══════════════════════════════════════════════════╝"
    for runner in "${RUNNERS[@]}"; do
        if bash "$runner"; then
            echo "  ✅ $(basename "$(dirname "$runner")") 通过"
        else
            echo "  ❌ $(basename "$(dirname "$runner")") 失败"
            OVERALL=1
        fi
    done
done

echo
if [ "$OVERALL" -eq 0 ]; then
    echo "══════════════ 全部测试套件通过 ══════════════"
else
    echo "══════════════ 存在失败套件(见上)══════════════"
fi
exit "$OVERALL"
