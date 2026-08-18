#!/usr/bin/env bash
# scripts/bench.sh —— M6 性能基线的 4 组微基准。
#
#   1. 冷启动           : `./reflect --version` 墙钟时间
#   2. 首事件           : `./reflect exec "noop"` → 首行 JSON 输出
#   3. 单轮对话         : mock LLM,200-token 回复,跑 5 次取均值
#   4. RSS              : `/usr/bin/time -l ./reflect exec "noop"` 峰值 RSS
#
# 输出:
#   target/bench/<name>.txt   — 原始 `/usr/bin/time -l` 输出行
#   target/bench/summary.txt  — 人类可读汇总
#   docs/benchmark.md         — 表格自动刷新于
#                               `<!-- BENCH:start -->` … `<!-- BENCH:end -->` 之间。
#
# 覆盖运行次数: `BENCH_RUNS=20 ./scripts/bench.sh`。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/reflect"
BENCHDIR="$ROOT/target/bench"
DOC="$ROOT/docs/benchmark.md"
SUMMARY="$BENCHDIR/summary.txt"
RUNS="${BENCH_RUNS:-5}"

mkdir -p "$BENCHDIR"

if [[ ! -x "$BIN" ]]; then
    echo "ERROR: $BIN not built. Run 'cargo build --release' first." >&2
    exit 1
fi

# 从 time(1) 输出提取 real/wall 秒。
# macOS `time -l`:`        0.69 real         0.00 user`(值在前,关键字在后)。
# GNU `time -v`:`        0.69 real ...` 或 `real 0m0.69s`。兼容两者。
stats_line() {
    local name="$1"
    local file="$BENCHDIR/${name}.txt"
    python3 - "$file" <<'PY' 2>/dev/null || echo "n/a"
import sys, re
text = open(sys.argv[1]).read()
vals = []
for line in text.splitlines():
    # macOS `time -l`:` 0.69 real 0.00 user`
    m = re.match(r"\s*([\d.]+)\s+real\b", line)
    if m:
        vals.append(float(m.group(1)))
        continue
    # GNU `time -p`:`real 0.69` / `real 0m0.69s`
    m = re.match(r"real\s+([\d.]+)", line)
    if m:
        vals.append(float(m.group(1)))
        continue
    m = re.match(r"real\s+(\d+)m([\d.]+)s", line)
    if m:
        vals.append(int(m.group(1)) * 60 + float(m.group(2)))
if not vals:
    print("n/a")
else:
    vals.sort()
    mean = sum(vals) / len(vals)
    print(f"min={vals[0]:.3f}s mean={mean:.3f}s max={vals[-1]:.3f}s n={len(vals)}")
PY
}

# 提取峰值 RSS(单位 KB)。macOS `maximum resident set size` 给的是字节。
extract_rss_kb() {
    local file="$1"
    local bytes
    bytes=$(awk '/maximum resident set size/ { print $1; exit }' "$file" 2>/dev/null)
    if [[ -z "$bytes" || "$bytes" == "0" ]]; then
        echo "0"
    else
        echo $((bytes / 1024))
    fi
}

run_bench() {
    local name="$1"; shift
    local out="$BENCHDIR/${name}.txt"
    : > "$out"
    for _ in $(seq 1 "$RUNS"); do
        /usr/bin/time -l "$@" 2>>"$out" >/dev/null || true
    done
    echo "[ok] $name → $out"
}

echo "Reflect bench ($(date -u +%Y-%m-%dT%H:%M:%SZ))"
echo "runs=$RUNS bin=$BIN"
echo

# 1. 冷启动
run_bench "cold_start" "$BIN" --version
# 2. 首事件
run_bench "first_event" "$BIN" exec "noop"
# 3. 单轮对话(mock provider,避免网络)
run_bench "single_turn" \
    env OPENAI_API_KEY=sk-test REFLECT_MODEL=mock "$BIN" exec "noop"
# 4. 真实单轮的 RSS(mock provider)
run_bench "rss" \
    env OPENAI_API_KEY=sk-test REFLECT_MODEL=mock "$BIN" exec "noop"

# 汇总写到 target/bench/summary.txt + stdout。
{
    echo "Reflect bench summary"
    echo "generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "runs: $RUNS"
    echo
    for name in cold_start first_event single_turn rss; do
        echo "$name: $(stats_line "$name")"
        if [[ "$name" == "rss" ]]; then
            echo "  peak_rss_kb: $(extract_rss_kb "$BENCHDIR/rss.txt")"
        fi
    done
} | tee "$SUMMARY"

# 自动刷新 docs/benchmark.md 的 BENCH 表(若有标记段)。
if [[ -f "$DOC" ]] && grep -q '<!-- BENCH:start -->' "$DOC"; then
    TABLE=$(
        cat <<EOF
| benchmark | result |
|-----------|--------|
| cold_start | $(stats_line cold_start) |
| first_event | $(stats_line first_event) |
| single_turn | $(stats_line single_turn) |
| rss | peak $(extract_rss_kb "$BENCHDIR/rss.txt") KB |
EOF
    )
    python3 - "$DOC" "$TABLE" <<'PY'
import sys, pathlib
path, table = pathlib.Path(sys.argv[1]), sys.argv[2]
text = path.read_text()
start, end = "<!-- BENCH:start -->", "<!-- BENCH:end -->"
if start in text and end in text:
    pre, rest = text.split(start, 1)
    _, post = rest.split(end, 1)
    path.write_text(pre + start + "\n" + table + "\n" + end + post)
PY
    echo "[ok] updated $DOC"
else
    echo "[skip] $DOC missing BENCH markers — add <!-- BENCH:start --> / <!-- BENCH:end -->"
fi

echo
echo "Summary: $SUMMARY"
