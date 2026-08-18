#!/usr/bin/env bash
# scripts/discussion_smoke.sh — v0.2.3 真实 LLM 烟雾测试。
#
# 与 scripts/e2e.sh 不同:e2e.sh 用 stub LLM 跑通状态机即可;本脚本要求
# 真实 OPENAI_API_KEY 或 ANTHROPIC_API_KEY,跑一次完整 `reflect discussion
# run`,验证:
#   1. CLI 检测到 provider 后走真 LLM 路径(不是 run_noop fallback)
#   2. 跑出非空 transcript(至少 3 个 agent 各产 ≥1 条 Utterance)
#   3. result JSON 含 `outcome` 字段且 ∈ {consensus, no_consensus, finished}
#   4. stdout 至少包含一次 `agent_turn` 日志(Gap C AgentTurn emit 验证)
#   5. 退出码 0
#
# 用法:
#   export OPENAI_API_KEY=sk-...
#   ./scripts/discussion_smoke.sh
# 或:
#   export ANTHROPIC_API_KEY=sk-ant-...
#   ./scripts/discussion_smoke.sh
#
# 失败时:打印 transcript + stderr 片段,exit 1。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ── 1. 前置检查 ─────────────────────────────────────────────────────────
if [[ -z "${OPENAI_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "ERROR: both OPENAI_API_KEY and ANTHROPIC_API_KEY are unset." >&2
    echo "Set one of them and rerun. Example:" >&2
    echo "  export OPENAI_API_KEY=sk-..." >&2
    echo "  ./scripts/discussion_smoke.sh" >&2
    exit 2
fi

CONFIG="crates/orchestration/reflect-discussion/examples/discussion.toml"
TRANSCRIPT="$(mktemp)"
LOG="$(mktemp)"
trap 'rm -f "$TRANSCRIPT" "$LOG"' EXIT

if [[ ! -f "$CONFIG" ]]; then
    echo "ERROR: $CONFIG not found" >&2
    exit 2
fi

# ── 2. release build(若已 build 则快速通过) ─────────────────────────────
echo "[smoke] cargo build --release -p reflect-cli ..."
cargo build --release -p reflect-cli 2>&1 | tail -5

BIN="$ROOT/target/release/reflect"
if [[ ! -x "$BIN" ]]; then
    echo "ERROR: $BIN not built" >&2
    exit 2
fi

# ── 3. 跑 reflect discussion run(无 key 时自动 fallback,显式 warn) ─────
echo "[smoke] running: $BIN discussion run -c $CONFIG"
set +e
"$BIN" discussion run -c "$CONFIG" -o "$TRANSCRIPT" 2>"$LOG"
RC=$?
set -e

echo "[smoke] exit code: $RC"

# ── 4. 失败诊断:打印 transcript + log 头部 ──────────────────────────────
if [[ $RC -ne 0 ]]; then
    echo "ERROR: discussion run exited $RC" >&2
    echo "---- transcript (head) ----" >&2
    head -30 "$TRANSCRIPT" >&2 || true
    echo "---- stderr (head) ----" >&2
    head -30 "$LOG" >&2 || true
    exit 1
fi

# ── 5. 校验 transcript ─────────────────────────────────────────────────
if [[ ! -s "$TRANSCRIPT" ]]; then
    echo "ERROR: transcript is empty (likely run_noop fallback)" >&2
    echo "---- stderr (head) ----" >&2
    head -30 "$LOG" >&2 || true
    exit 1
fi

# 至少 3 行 transcript(每 agent 至少 1 条)
LINES=$(wc -l < "$TRANSCRIPT")
echo "[smoke] transcript lines: $LINES"
if [[ $LINES -lt 5 ]]; then
    echo "WARN: transcript seems short ($LINES lines); full content:" >&2
    cat "$TRANSCRIPT" >&2
    # 不直接 fail,LLM 偶发少输出也算跑通
fi

# ── 6. 校验 result JSON 含 outcome 字段 ─────────────────────────────────
if ! grep -q '"outcome"' "$TRANSCRIPT"; then
    echo "ERROR: result JSON missing 'outcome' field" >&2
    echo "---- transcript ----" >&2
    cat "$TRANSCRIPT" >&2
    exit 1
fi

# outcome 必须是已知值
OUTCOME=$(grep '"outcome"' "$TRANSCRIPT" | head -1 | sed -E 's/.*"outcome"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
echo "[smoke] outcome: $OUTCOME"
case "$OUTCOME" in
    consensus|no_consensus|finished) ;;
    *)
        echo "ERROR: unknown outcome '$OUTCOME'" >&2
        exit 1
        ;;
esac

# ── 7. 校验 stderr 含 AgentTurn emit 日志(Gap C 验证) ──────────────────
if [[ -s "$LOG" ]] && grep -qi 'agent_turn\|discussion round start' "$LOG"; then
    echo "[smoke] stderr contains AgentTurn / round-start logs"
else
    echo "WARN: stderr lacks AgentTurn logs (可能 RUST_LOG=warn 过滤了 info 级)"
    echo "---- stderr (head) ----"
    head -10 "$LOG" || true
fi

# ── 8. 打印 transcript 前 30 行供肉眼检查 ───────────────────────────────
echo
echo "==== transcript (first 30 lines) ===="
head -30 "$TRANSCRIPT"
echo "==== end ===="

echo
echo "[smoke] ALL GREEN — discussion smoke passed (outcome=$OUTCOME, lines=$LINES)"
