#!/usr/bin/env bash
# scripts/e2e.sh —— 端到端冒烟测试。
#
# 执行步骤:
#   1. cargo build --release
#   2. cargo doc --no-deps
#   3. 5 个 example(离线,mock provider)
#   4. headless exec(mock provider)→ JSONL 输出(jq 过滤 TurnComplete)
#   5. cargo test --workspace --all-targets
#   6. clippy + fmt 严格检查
#   7. (M7) 品牌残留 grep = 0
#   8. (M7) config watcher 在 TOML 变更时触发
#   9. (M9) discussion 冒烟 —— `reflect discussion run` + discussion_demo example
#  10. (v0.4) login 冒烟 —— `reflect login --force` 写盘
#  11. (v0.4) doctor 冒烟 —— 空 config 下干净退出
#  12. (v1.3) serve 冒烟 —— `reflect serve` + mock provider:
#      远程工具注册 / 请求 / 回执 / turn_complete / shutdown 全链路
#  13. (v1.3) SDK 冒烟 —— Python / TypeScript SDK 对真实二进制的
#      离线全链路(握手 / 多轮上下文 / 自定义工具客户端执行)
#
# 注:本仓库二进制不含 `tui` 子命令(TUI 已拆分至独立仓库 Reflect-CLI),
# 旧版步骤 3 / 13 的 tui 冒烟已移除 —— 它们恒失败(子命令不存在)却被
# `|| true` 吞掉,是不产生任何断言的假阳性步骤。
#
# 前置依赖:`jq`(步骤 4 / 12 的 JSONL 字段断言用;与 `tests/rust/*.sh`
# 同一依赖,CI 里由 `apt-get install -y jq` 提供)、`node`(步骤 13 的
# SDK 冒烟)、`python3` + pytest(步骤 5 / 13 的 SDK 用例)。
#
# 每步快速失败。退出码 0 = 全部通过。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/reflect"
export BIN  # 内层 bash -c 步骤(login/doctor 等)需要继承
LOG="$ROOT/target/e2e.log"
mkdir -p "$(dirname "$LOG")"

step() {
    local name="$1"; shift
    echo
    echo "=========================================="
    echo "  $name"
    echo "=========================================="
    "$@" 2>&1 | tee -a "$LOG"
}

# 1. release 构建
step "1/13 release build" \
    cargo build --release

# 2. cargo doc --no-deps(只文档化本 crate)
step "2/13 cargo doc --no-deps" \
    cargo doc --no-deps

# 3. 5 个 example(REFLECT_MODEL=mock 跳过真实网络;mock 下应当全绿,不吞错误)
#    review 修复:此前 `|| true` 让 example 失败也判过 —— 冒烟形同虚设。
step "3/13 5 examples (mock provider, offline)" \
    bash -c '
        set -euo pipefail
        for ex in headless_run custom_tool multi_turn custom_provider hook_listener; do
            echo "--- example: $ex"
            case "$ex" in
                # headless_run 需要一个 prompt 参数(见
                # crates/runtime/reflect/examples/headless_run.rs:缺失即报
                # "usage: headless_run <prompt>")。`--` 之后的参数透传给 example。
                headless_run)
                    REFLECT_MODEL=mock cargo run --quiet --release --example "$ex" -- "hello from e2e"
                    ;;
                *)
                    REFLECT_MODEL=mock cargo run --quiet --release --example "$ex"
                    ;;
            esac
        done
    '

# 4. headless exec(mock provider)→ JSONL,断言出现 TurnComplete。
#    review 修复:此前注入假 OPENAI_API_KEY 打真网络必然失败,再被 `||`
#    吞掉 —— 该核心断言从未真正生效。改走 mock,严格失败。
step "4/13 headless exec emits TurnComplete (mock provider)" \
    bash -c '
        set -euo pipefail
        TMP=$(mktemp -d)
        trap "rm -rf $TMP" EXIT
        cat > "$TMP/script.jsonl" <<EOF
{"type":"text","text":"hi from mock"}
EOF
        REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$TMP/script.jsonl" \
            "$BIN" exec "echo hi" > "$TMP/out.jsonl"
        jq -e "select(.msg.type == \"turn_complete\")" "$TMP/out.jsonl" >/dev/null \
            || { echo "FAIL: TurnComplete not seen in headless output"; cat "$TMP/out.jsonl"; exit 1; }
        echo "[ok] headless exec → TurnComplete"
    '

# 5. 全 workspace 测试(--all-targets 与 CI 对齐;同时保证 bins 先于测试构建,
#    mock_mcp_server 等测试依赖的辅助二进制不会因构建顺序竞态缺失)
step "5/13 cargo test --workspace --all-targets" \
    cargo test --workspace --all-targets --quiet

# 7. clippy + fmt 检查
step "6/13 clippy + fmt" \
    bash -c '
        cargo clippy --workspace --all-targets -- -D warnings
        cargo fmt --all -- --check
    '

# 8. (M7) 品牌残留 grep —— 必须为空。
#    迁移文档(docs/migration-m6-to-m7.md)与 CLAUDE.md 的 M7 进度行
#    作为映射元数据合法引用旧名;排除之。
#    e2e.sh 自身也含要扫的字面量(作为 scan 实现),必须 exclude 自己。
step "7/13 brand residual scan (legacy names)" \
    bash -c '
        if grep -rn "pureagent\|PureAgent\|PUREAGENT_" \
            crates/ scripts/ README.md .github/ \
            --include="*.rs" --include="*.toml" \
            --include="*.sh" --include="*.yml" --include="*.yaml" \
            --include="*.json" --include="*.snap" 2>/dev/null \
            | grep -v Cargo.lock \
            | grep -v "scripts/e2e.sh"; then
            echo "FAIL: residual pureagent strings in source/CI"
            exit 1
        fi
        # docs/ + CLAUDE.md 在映射上下文里合法引用旧名。
        echo "[ok] zero residual hits in source/CI"
    '

# 9. (M7) config watcher 端到端 —— 写配置,期望 registry 重建。
step "8/13 config watcher e2e" \
    bash -c '
        set -euo pipefail
        TMPDIR=$(mktemp -d)
        cat > "$TMPDIR/config.toml" <<EOF
[active]
provider = "anthropic"
[anthropic]
api_key = "sk-initial"
EOF
        # 跑一个内联小测试,覆盖 ConfigWatcher。
        cargo test --quiet -p reflect-exec --test config_reload 2>&1 | tail -10
        rm -rf "$TMPDIR"
    '

# 10. (M9) discussion 冒烟 —— `reflect discussion run` CLI + discussion_demo example。
step "9/13 discussion smoke (M9)" \
    bash -c '
        set -euo pipefail
        # 9a. `reflect discussion --help` 输出 Run / Ls
        ./target/release/reflect discussion --help | grep -iE "run|ls" >/dev/null || { echo "FAIL: discussion --help missing Run/Ls"; exit 1; }
        # 9b. `reflect discussion run -c <toml>` 跑通,输出 transcript + JSON result
        ./target/release/reflect discussion run -c crates/orchestration/reflect-discussion/examples/discussion.toml 2>&1 | grep -E "outcome|rounds_completed" >/dev/null || { echo "FAIL: discussion run missing outcome"; exit 1; }
        # 9c. discussion_demo example 跑通,输出 Started/Finished 事件
        cargo run --quiet -p reflect --example discussion_demo 2>&1 | grep -E "started|finished" >/dev/null || { echo "FAIL: discussion_demo missing started/finished"; exit 1; }
        # 9d. subagent 嵌套深度 3 集成测试通过(不写死用例数,避免测试增删误报)
        cargo test --quiet -p reflect-discussion --test subagent_nested_depth 2>&1 | tail -3 | grep -E "test result: ok" >/dev/null || { echo "FAIL: subagent_nested_depth integration test"; exit 1; }
        echo "[ok] discussion smoke all green"
    '

# 11. (v0.4) login smoke — 临时 HOME + `--force` 短 key 写盘,验证 TOML 落地。
step "10/13 login smoke (v0.4)" \
    bash -c '
        set -euo pipefail
        TMPDIR=$(mktemp -d)
        export HOME="$TMPDIR"
        # 用 --force 跳过长度校验,避免脚本里的短 key 被拒绝。
        "$BIN" login --provider anthropic --api-key "sk-test" --force >/dev/null
        # 验证 ~/.reflect/config.toml 存在 + 含 api_key
        [ -f "$TMPDIR/.reflect/config.toml" ] || { echo "FAIL: config.toml not written"; exit 1; }
        grep -q "sk-test" "$TMPDIR/.reflect/config.toml" || { echo "FAIL: api_key not in config.toml"; exit 1; }
        rm -rf "$TMPDIR"
        echo "[ok] login wrote ~/.reflect/config.toml"
    '

# 12. (v0.4) doctor smoke — 临时 HOME 无 config,`reflect doctor` 仍能跑通退出 0。
step "11/13 doctor smoke (v0.4)" \
    bash -c '
        set -euo pipefail
        TMPDIR=$(mktemp -d)
        export HOME="$TMPDIR"
        "$BIN" doctor >/dev/null || { echo "FAIL: doctor exited non-zero"; exit 1; }
        rm -rf "$TMPDIR"
        echo "[ok] doctor runs clean with empty config"
    '

# 14. (v1.3) serve 冒烟 —— 真二进制 `reflect serve` + mock provider:
#     fifo 驱动 stdin,断言 session_configured / 远程工具请求与回执 /
#     tool_call_end / turn_complete / shutdown 全链路(wire 协议见
#     sdks/PROTOCOL.md)。
step "12/13 serve smoke (mock provider, remote tool)" \
    bash -c '
        set -euo pipefail
        TMP=$(mktemp -d)
        trap "rm -rf $TMP" EXIT
        # mock 脚本:第 1 次模型调用发起远程工具调用,第 2 次回文本收尾。
        cat > "$TMP/script.jsonl" <<EOF
{"type":"tool_call","name":"e2e_echo","args":{"q":"hi"}}
{"type":"text","text":"tool result consumed"}
EOF
        FIFO="$TMP/serve_in"
        mkfifo "$FIFO"
        OUT="$TMP/serve_out.jsonl"
        REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$TMP/script.jsonl" \
            "$BIN" serve < "$FIFO" > "$OUT" 2>"$TMP/serve_err.log" &
        SERVE_PID=$!
        exec 3> "$FIFO"
        # 注册远程工具 + 提交一轮对话。
        echo "{\"id\":\"reg\",\"op\":{\"type\":\"register_tools\",\"tools\":[{\"name\":\"e2e_echo\",\"description\":\"e2e echo\",\"parameters\":{\"type\":\"object\"}}]}}" >&3
        echo "{\"id\":\"t1\",\"op\":{\"type\":\"user_input\",\"items\":[{\"type\":\"text\",\"text\":\"调工具\"}]}}" >&3
        # 轮询等远程工具请求,读到 call_id 后回执(模拟 SDK 本地执行)。
        deadline=$((SECONDS + 30))
        CALL_ID=""
        while [ $SECONDS -lt $deadline ]; do
            if grep -q "\"tool_execution_request\"" "$OUT" 2>/dev/null; then
                CALL_ID=$(jq -r "select(.msg.type==\"tool_execution_request\") | .msg.call_id" "$OUT" | head -1)
                break
            fi
            sleep 0.2
        done
        [ -n "$CALL_ID" ] || { echo "FAIL: no tool_execution_request"; cat "$OUT"; exit 1; }
        printf "{\"id\":\"resp\",\"op\":{\"type\":\"tool_execution_response\",\"call_id\":\"%s\",\"output\":{\"content\":[{\"type\":\"text\",\"text\":\"echo-back\"}],\"is_error\":false,\"metadata\":{},\"elapsed_ms\":1}}}\n" "$CALL_ID" >&3
        # 等 turn 完成(工具结果回流 + 收尾文本)。
        while [ $SECONDS -lt $deadline ]; do
            grep -q "\"turn_complete\"" "$OUT" 2>/dev/null && break
            sleep 0.2
        done
        grep -q "\"turn_complete\"" "$OUT" || { echo "FAIL: no turn_complete"; cat "$OUT"; exit 1; }
        # 优雅退出。
        echo "{\"id\":\"bye\",\"op\":{\"type\":\"shutdown\"}}" >&3
        exec 3>&-
        wait $SERVE_PID
        # wire 断言:握手 / 工具请求 / 工具结果 / 终态。
        jq -e "select(.msg.type==\"session_configured\")" "$OUT" >/dev/null || { echo "FAIL: no session_configured"; exit 1; }
        jq -e "select(.msg.type==\"tool_call_end\" and .msg.is_error==false)" "$OUT" >/dev/null || { echo "FAIL: no successful tool_call_end"; exit 1; }
        jq -e "select(.msg.type==\"tool_call_end\" and (.msg.output | tostring | contains(\"echo-back\")))" "$OUT" >/dev/null || { echo "FAIL: echo-back not in tool output"; exit 1; }
        jq -e "select(.msg.type==\"shutdown_complete\")" "$OUT" >/dev/null || { echo "FAIL: no shutdown_complete"; exit 1; }
        echo "[ok] serve smoke: handshake + remote tool + turn_complete + shutdown"
    '

# 15. (v1.3) SDK 冒烟 —— Python / TypeScript SDK 对真实二进制的
#     离线全链路(握手 / 多轮上下文 / 自定义工具客户端执行)。
step "13/13 sdk smoke (python + typescript, mock provider)" \
    bash -c '
        set -euo pipefail
        # TS SDK 需要 node + 已构建的 dist;Python 需要 python3 + pytest 之外零依赖。
        command -v node >/dev/null || { echo "SKIP: node not available"; exit 0; }
        ( cd sdks/typescript && npm run build --silent )
        ./scripts/sdk_smoke.sh
    '

echo
echo "=========================================="
echo "  全部端到端检查通过"
echo "=========================================="
echo "log: $LOG"