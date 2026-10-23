#!/usr/bin/env bash
# scripts/ofm_gateway.sh —— dsh-our-free-model 本地网关的启停助手。
#
# 该网关是「OpenCode inference 车道」的前置:那条车道有 **v2 客户端指纹门**
# (UA / 会话头 / 铸造 session id / 逐字 read+shell 工具定义 / 禁
# `tool_choice:none`),只有网关会补齐指纹。Kilo 免费池**不需要**它。
#
# ⚠️ 本脚本**不执行 `npm install`** —— 安装依赖会修改宿主状态,必须由
# 用户显式触发。脚本只做启停与状态查询。
#
# 用法:
#   scripts/ofm_gateway.sh status     # 查询状态(默认)
#   scripts/ofm_gateway.sh start      # 后台启动(需先自行 npm install)
#   scripts/ofm_gateway.sh stop       # 停止
#
# 仓库路径可用 OFM_REPO 环境变量覆盖(默认按同级 ../dsh-our-free-model 找)。

set -u

REPO="${OFM_REPO:-}"
PORT="${OFM_PORT:-18900}"
LOG="${OFM_LOG:-${TMPDIR:-/tmp}/ofm-gateway.log}"

if [ -z "$REPO" ]; then
    for candidate in \
        "$HOME/Code/Github/dsh-our-free-model" \
        "$HOME/Code/Github/dsh-our-free-model-main" \
        "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/../dsh-our-free-model"; do
        if [ -d "$candidate" ]; then REPO="$candidate"; break; fi
    done
fi

ACTION="${1:-status}"

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

case "$ACTION" in
    -h|--help) usage; exit 0 ;;
esac

if [ -z "$REPO" ] || [ ! -d "$REPO" ]; then
    echo "找不到 dsh-our-free-model 仓库;用 OFM_REPO=/path/to/repo 指定" >&2
    exit 2
fi

PIDFILE="$REPO/.ofm-gateway.pid"

running_pid() {
    [ -f "$PIDFILE" ] || return 1
    local pid
    pid="$(cat "$PIDFILE" 2>/dev/null)"
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    echo "$pid"
}

case "$ACTION" in
    status)
        pid="$(running_pid || true)"
        if [ -n "$pid" ]; then
            echo "网关:运行中(pid=$pid,端口=$PORT)"
            echo "接入配置:"
            echo "  base_url = \"http://127.0.0.1:$PORT\""
            echo "  api_key  = <~/.our-free-model/settings.json 的 forwardKey>"
        else
            echo "网关:未运行"
            echo "启动方式(需先自行执行 npm install):"
            echo "  cd $REPO && npm run start:standalone -- --port $PORT"
            echo "或直接:$0 start"
        fi
        ;;
    start)
        pid="$(running_pid || true)"
        if [ -n "$pid" ]; then
            echo "网关已在运行(pid=$pid)"
            exit 0
        fi
        if [ ! -d "$REPO/node_modules" ]; then
            echo "FAIL: $REPO/node_modules 不存在" >&2
            echo "请先自行执行:cd $REPO && npm install" >&2
            echo "(本脚本不自动安装依赖 —— 安装会修改宿主状态,需显式触发)" >&2
            exit 3
        fi
        echo "启动网关(端口=$PORT,日志=$LOG)..."
        ( cd "$REPO" && nohup npm run start:standalone -- --port "$PORT" >"$LOG" 2>&1 & echo $! > "$PIDFILE" )
        sleep 3
        pid="$(running_pid || true)"
        if [ -n "$pid" ]; then
            echo "网关:已启动(pid=$pid)"
        else
            echo "FAIL: 启动失败,见日志 $LOG" >&2
            tail -20 "$LOG" 2>/dev/null >&2
            exit 1
        fi
        ;;
    stop)
        pid="$(running_pid || true)"
        if [ -z "$pid" ]; then
            echo "网关:未运行"
            exit 0
        fi
        kill "$pid" 2>/dev/null && echo "网关:已停止(pid=$pid)"
        rm -f "$PIDFILE"
        ;;
    *)
        echo "未知动作:$ACTION(可选:status / start / stop)" >&2
        usage >&2
        exit 2
        ;;
esac
