#!/usr/bin/env bash
# scripts/free_model_practice.sh —— 免费模型横向对比演练。
#
# 跑一组真实场景 × 从 provider 接口取到的多个模型,输出 Markdown 对比表
# (总耗时 / token / 工具调用数 / 终态 / 错误分类)。
#
# **不是门禁**:退出码只反映脚本自身错误;模型表现差异只体现在报告里。
#
# 模型来源:`zhuye models list --free-only --json`(接口实时拉取),
# 不写死模型 id —— 免费模型集合由上游决定,随时可能增删。
#
# ⚠️ 上游可能记录 prompt:脚本只用合成数据,不要把真实仓库内容 / 凭据
# 喂给免费池。
#
# 用法:
#   scripts/free_model_practice.sh                       # 默认车道 + 前 3 个模型
#   scripts/free_model_practice.sh --models a,b,c        # 指定模型
#   scripts/free_model_practice.sh --count 2              # 从清单取 2 个
#   scripts/free_model_practice.sh --out report.md        # 报告落盘
#   scripts/free_model_practice.sh --dry-run              # 只列计划,不真跑

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

BIN="${ZHUYE_BIN:-$ROOT/target/release/zhuye}"
BASE_URL="${ZHUYE_LIVE_BASE_URL:-https://api.kilo.ai/api/gateway}"
API_KEY="${ZHUYE_LIVE_API_KEY:-}"
COUNT="${ZHUYE_LIVE_MODEL_COUNT:-3}"
MODELS=""
OUT=""
DRY_RUN=0

while [ $# -gt 0 ]; do
    case "$1" in
        --models) MODELS="${2:-}"; shift 2 ;;
        --count)  COUNT="${2:-3}"; shift 2 ;;
        --out)    OUT="${2:-}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "未知参数:$1" >&2; exit 2 ;;
    esac
done

if [ ! -x "$BIN" ]; then
    echo "FAIL: $BIN 不存在;先执行 cargo build --release -p zhuye-cli" >&2
    exit 2
fi

# 隔离环境:不污染真实 ~/.zhuye。
ISO_HOME="$(mktemp -d "${TMPDIR:-/tmp}/zhuye-practice-home.XXXXXX")"
ISO_CWD="$(mktemp -d "${TMPDIR:-/tmp}/zhuye-practice-cwd.XXXXXX")"
REPORT_FILE="$ISO_CWD/report.md"
cleanup() { rm -rf "$ISO_HOME" "$ISO_CWD"; }
trap cleanup EXIT

mkdir -p "$ISO_HOME/.zhuye"
cat > "$ISO_HOME/.zhuye/config.toml" <<EOF
# 免费模型演练配置(合成数据专用)
[hooks]
enabled = []

[active]
provider = "openai"

[openai]
api_key = "$API_KEY"
base_url = "$BASE_URL"
timeout_secs = 300
model = "free"

[[openai.credentials]]
label = "practice"
api_key = "$API_KEY"
base_url = "$BASE_URL"
model = "free"
EOF

echo "═══ 免费模型横向对比演练 ═══"
echo "车道:     $BASE_URL"
echo "模型来源: provider 清单接口(实时拉取)"
echo

# ── 模型清单 ────────────────────────────────────────────────────────────────
if [ -n "$MODELS" ]; then
    MODEL_LIST="$MODELS"
else
    MODEL_LIST="$(HOME="$ISO_HOME" "$BIN" models list --free-only --json 2>/dev/null \
        | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
ids = sorted(m.get('id', '') for m in data.get('models', []) if m.get('id'))
print(','.join(ids[:$COUNT]))
" 2>/dev/null)"
fi

if [ -z "$MODEL_LIST" ]; then
    echo "FAIL: 拿不到模型清单(检查 ZHUYE_LIVE_BASE_URL / 出口地区 / 配额)" >&2
    exit 3
fi

echo "模型:     $MODEL_LIST"
echo

if [ "$DRY_RUN" = "1" ]; then
    echo "(dry-run:不发起推理请求)"
    exit 0
fi

# ── 场景集(合成数据,不含任何真实仓库内容)────────────────────────────────
# 场景名|给模型的指令|期望工具调用数
SCENES='单轮问答|请用一句话回答:1+1 等于几? 不要调用任何工具。|0
内置工具|请用 bash 工具执行 echo practice-probe,然后把输出原样告诉我。|1
多轮上下文|记住这个数字 4242,只回复「记住了」四个字。|0
失败恢复|请调用 definitely_not_a_registered_tool 工具。|1'

printf '| 模型 | 场景 | 终态 | 总耗时(ms) | 输入 tok | 输出 tok | 工具调用 | 错误 |\n' > "$REPORT_FILE"
printf '|---|---|---|---|---|---|---|---|\n' >> "$REPORT_FILE"

for model in ${MODEL_LIST//,/ }; do
    echo "── 模型: $model"
    while IFS='|' read -r scene instruction _rest; do
        [ -z "$scene" ] && continue
        # 每个场景清一次会话,避免上下文串场。
        rm -rf "$ISO_HOME/.zhuye/sessions"
        EVENTS="$ISO_CWD/events.jsonl"
        : > "$EVENTS"

        START=$(python3 -c 'import time;print(int(time.time()*1000))')
        ( cd "$ISO_CWD" && HOME="$ISO_HOME" "$BIN" exec "$instruction" 2>/dev/null > "$EVENTS" )
        END=$(python3 -c 'import time;print(int(time.time()*1000))')

        python3 - "$EVENTS" "$model" "$scene" "$START" "$END" >> "$REPORT_FILE" <<'PYEOF'
import json, sys
events_path, model, scene, start, end = sys.argv[1:6]
start, end = int(start), int(end)
status, err, tools = "", "", 0
usage = {}
try:
    with open(events_path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                m = json.loads(line).get("msg", {})
            except Exception:
                continue
            t = m.get("type", "")
            if t == "tool_call_begin":
                tools += 1
            elif t == "turn_complete":
                status = m.get("status", "")
                usage = m.get("usage") or {}
            elif t in ("error", "stream_error"):
                err = f"{m.get('code', '')} {m.get('message', '')}".strip()[:48]
except FileNotFoundError:
    pass
print(
    f"| {model} | {scene} | {status or '-'} | {end - start} | "
    f"{usage.get('input_tokens', 0)} | {usage.get('output_tokens', 0)} | "
    f"{tools} | {err or '-'} |"
)
PYEOF
    done <<< "$SCENES"
done

echo
cat "$REPORT_FILE"

if [ -n "$OUT" ]; then
    cp "$REPORT_FILE" "$OUT" && echo && echo "报告已写入: $OUT"
fi

exit 0
