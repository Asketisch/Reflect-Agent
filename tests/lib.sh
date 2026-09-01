#!/usr/bin/env bash
# tests/lib.sh —— 测试套件公共助手。
#
# 所有 tests/ 下的脚本 source 本文件,获得:
#   - 隔离环境:mktemp HOME + 独立 cwd,不污染真实 ~/.reflect;
#   - mock provider 环境(REFLECT_MODEL=mock,离线、免 API key);
#   - 断言助手(assert_ok / assert_fail / assert_contains / assert_json);
#   - 便携超时(macOS 无 timeout/gtimeout,用后台 pid + kill 模拟);
#   - 用例统计与失败汇总(tcase / tfail 汇总退出码)。
#
# 约定:注释一律中文;脚本自身可独立执行,退出码 0 = 全部通过。

# ── 仓库根与二进制定位 ────────────────────────────────────────────────────
TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$TESTS_DIR/.." && pwd)"
REFLECT_BIN="${REFLECT_BIN:-$ROOT/target/release/reflect}"

# ── 全局状态 ──────────────────────────────────────────────────────────────
declare -i PASS_COUNT=0
declare -i FAIL_COUNT=0
declare -a FAIL_NAMES=()

# tcase <名称> <命令...>:执行一条用例,记录通过/失败。
tcase() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        PASS_COUNT+=1
        echo "  [ok]   $name"
    else
        FAIL_COUNT+=1
        FAIL_NAMES+=("$name")
        echo "  [FAIL] $name"
    fi
}

# tfinish:打印汇总并返回汇总退出码(0=全过)。
tfinish() {
    local script_name="${1:-$(basename "${BASH_SOURCE[1]:-unknown}")}"
    echo
    echo "──────────────────────────────────────────"
    echo "  $script_name: $PASS_COUNT passed, $FAIL_COUNT failed"
    if [ "$FAIL_COUNT" -gt 0 ]; then
        printf '  失败用例: %s\n' "${FAIL_NAMES[*]}"
        return 1
    fi
    return 0
}

# ── 断言助手(供脚本内的函数体使用,配合 tcase)───────────────────────────

# assert_contains <名称> <子串> <文件>:文件包含子串。
assert_contains() {
    local name="$1" needle="$2" file="$3"
    grep -qF -- "$needle" "$file"
}

# assert_not_contains <名称> <子串> <文件>:文件不含子串。
assert_not_contains() {
    local name="$1" needle="$2" file="$3"
    ! grep -qF -- "$needle" "$file"
}

# assert_json <名称> <jq 表达式> <jsonl 文件>:对 JSONL 逐行过滤后至少命中 1 条。
# 例:assert_json "exec 发出 turn_complete" 'select(.msg.type=="turn_complete")' out.jsonl
assert_json() {
    local name="$1" expr="$2" file="$3"
    jq -e "$expr" "$file" >/dev/null 2>&1
}

# ── 便携超时:run_with_timeout <秒> <命令...> ─────────────────────────────
# macOS 无 coreutils timeout;后台起进程,轮询等待,超时 SIGTERM+SIGKILL。
run_with_timeout() {
    local secs="$1"; shift
    "$@" &
    local pid=$!
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$secs" ]; then
            kill -TERM "$pid" 2>/dev/null
            sleep 1
            kill -9 "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null
            return 124
        fi
        sleep 1
        waited=$((waited + 1))
    done
    wait "$pid"
}

# ── 隔离环境 ──────────────────────────────────────────────────────────────
# make_isolated_env:创建并进入隔离环境,设置:
#   ISO_HOME / ISO_CWD(导出 HOME、cd 进去)
#   预写 ~/.reflect/config.toml —— [hooks] enabled = [] 关闭全部内置 hook。
#   说明:exec 默认启用 VerificationHook(Stop 时跑 `cargo test`)与
#   PlanCompletionHook(3 次否决),单轮耗时会到分钟级;测试只验功能
#   语义,不验 hook 计时,统一在配置里显式关闭。config.toml 必须存在,
#   否则 ConfigWatcher 报错直接退出。
make_isolated_env() {
    ISO_HOME="$(mktemp -d "${TMPDIR:-/tmp}/reflect-test-home.XXXXXX")"
    ISO_CWD="$(mktemp -d "${TMPDIR:-/tmp}/reflect-test-cwd.XXXXXX")"
    mkdir -p "$ISO_HOME/.reflect"
    cat > "$ISO_HOME/.reflect/config.toml" <<'EOF'
# 测试用最小配置:关闭全部内置 hook(见 make_isolated_env 注释)
[hooks]
enabled = []
EOF
    export HOME="$ISO_HOME"
    cd "$ISO_CWD" || exit 1
}

# cleanup_isolated_env:清掉隔离目录(脚本 trap 里调用)。
cleanup_isolated_env() {
    [ -n "${ISO_HOME:-}" ] && rm -rf "$ISO_HOME"
    [ -n "${ISO_CWD:-}" ] && rm -rf "$ISO_CWD"
}

# ── mock provider 环境 ────────────────────────────────────────────────────
# mock_env <mock 脚本路径(可空)>:以命令前缀形式输出 mock 环境变量。
#   用法:mock_env "$TMP/script.jsonl" "$BIN" exec "hi" > out.jsonl
# mock 脚本为 JSONL,每行一次模型回复:
#   {"type":"text","text":"回复"}            —— 纯文本
#   {"type":"tool_call","name":"t","args":{}} —— 发起工具调用
mock_env() {
    local script="${1:-}"
    if [ -n "$script" ]; then
        echo "env" "REFLECT_MODEL=mock" "REFLECT_MOCK_SCRIPT=$script"
    else
        echo "env" "REFLECT_MODEL=mock"
    fi
}

# make_mock_script <输出路径> <行...>:写一个 mock 脚本文件。
make_mock_script() {
    local path="$1"; shift
    : > "$path"
    local line
    for line in "$@"; do
        printf '%s\n' "$line" >> "$path"
    done
}

# require_binary:确认 release 二进制存在,否则提示构建命令后退出。
require_binary() {
    if [ ! -x "$REFLECT_BIN" ]; then
        echo "FAIL: $REFLECT_BIN 不存在;先执行 cargo build --release -p reflect-cli" >&2
        exit 2
    fi
}

# require_cmd <命令...>:缺少任一依赖命令则退出(提示 SKIP 语义由调用方处理)。
require_cmd() {
    local c
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || return 1
    done
    return 0
}
