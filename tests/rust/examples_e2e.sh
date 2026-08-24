#!/usr/bin/env bash
# tests/rust/examples_e2e.sh —— 库门面(reflect crate)examples 冒烟。
#
# 全部以 REFLECT_MODEL=mock 离线运行,断言退出码 0 + 关键输出:
#   headless_run     —— 最小单轮(headless 入门)
#   custom_tool      —— 自定义工具注册
#   multi_turn       —— 多轮对话
#   custom_provider  —— 自定义 provider 接入
#   hook_listener    —— ToolExec hook 拦截
#   discussion_demo  —— discussion 编排(lib API 路径)
#
# 用法:由 tests/rust/run.sh source;也可独立执行。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

EX_BIN_DIR="$ROOT/target/release/examples"

examples_e2e() {
    make_isolated_env
    trap cleanup_isolated_env RETURN
    # examples 走与 exec 相同的 env 兜底:mock provider 离线注册;
    # 部分 example 自带「必须显式给 key」的前置检查(不认 REFLECT_MODEL),
    # 补一个 dummy OPENAI_API_KEY 让 precheck 通过,实际仍走 mock。
    export REFLECT_MODEL=mock
    export OPENAI_API_KEY="${OPENAI_API_KEY:-e2e-dummy-key}"

    # examples 需先构建(存在即跳过)
    if [ ! -x "$EX_BIN_DIR/headless_run" ]; then
        cargo build --release -p reflect --examples --quiet
    fi

    # headless_run 需要 prompt 参数
    "$EX_BIN_DIR/headless_run" "e2e prompt" > ex1.out 2>ex1.err
    tcase "example headless_run 退出 0" test $? -eq 0

    "$EX_BIN_DIR/custom_tool" > ex2.out 2>ex2.err
    tcase "example custom_tool 退出 0" test $? -eq 0

    "$EX_BIN_DIR/multi_turn" > ex3.out 2>ex3.err
    tcase "example multi_turn 退出 0" test $? -eq 0
    tcase "example multi_turn 打印两轮 USER" sh -c "grep -q 'USER 1' ex3.out && grep -q 'USER 2' ex3.out"

    "$EX_BIN_DIR/custom_provider" > ex4.out 2>ex4.err
    tcase "example custom_provider 退出 0" test $? -eq 0
    tcase "example custom_provider mock 回复" grep -q "Hello from mock LLM" ex4.out

    "$EX_BIN_DIR/hook_listener" > ex5.out 2>ex5.err
    tcase "example hook_listener 退出 0" test $? -eq 0
    tcase "example hook_listener 打印 hook 拦截" grep -q "Denied by hook" ex5.out

    "$EX_BIN_DIR/discussion_demo" > ex6.out 2>ex6.err
    tcase "example discussion_demo 退出 0" test $? -eq 0
    tcase "example discussion_demo 打印 started" grep -q "started" ex6.out
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
examples_e2e

tfinish "examples_e2e.sh" || exit 1
exit 0
