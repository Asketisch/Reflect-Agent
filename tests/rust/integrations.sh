#!/usr/bin/env bash
# tests/rust/integrations.sh —— 集成面深度使用测试(非冒烟)。
#
# 覆盖 CLI/serve 之上的集成能力,全部走真实产物(release 二进制 +
# mock_mcp_server 辅助进程),mock LLM 离线:
#   mcp        MCP server 生命周期 + in-turn 工具调用全链路
#              (mcp_server_started / mcp_server_failed / mcp_tool_invoked
#               / tool_call_end 回传 echo 输出)
#   skill      工作区 SKILL.md 扫描 + load_skill 工具激活(正文 + 工具列表)
#   notes      add_session_note 工具 → $HOME/.reflect/session-notes/<thread>.jsonl 落盘
#   bubble     PermissionMode::Bubble 非阻塞通知 + 自动放行
#              (permission_mode_changed / permission_bubble / 工具照常执行)
#   hotreload  配置热重载(config_reloaded 事件 + path + sections_changed)
#   plugin     marketplace add/install/enable → 启动期 plugin_loaded 事件
#
# 前置:release 二进制 + target/debug/mock_mcp_server(不存在时 mcp/bubble
#       组跳过并计为失败之外的 SKIP 说明)。全程 REFLECT_MODEL=mock + 隔离 HOME。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary
require_cmd jq || { echo "FAIL: 需要 jq" >&2; exit 2; }
MOCK_MCP="$ROOT/target/debug/mock_mcp_server"
have_mock_mcp() { [ -x "$MOCK_MCP" ]; }
if ! have_mock_mcp; then
    echo "NOTE: $MOCK_MCP 不存在,先执行 cargo build -p reflect-mcp" >&2
fi

# ── 轮询助手(与 serve_ops.sh 同模式)────────────────────────────────────
# waitfor <file> <pattern> [timeout]:轮询文件直到出现 pattern(grep -q),
# 超时返回 1。serve 事件流是 append-only JSONL,增量 grep 安全。
# 注意:deadline 必须单独一行 local —— 同一条 local 语句里右侧全部先展开
# 再赋值,`local to=... d=$((SECONDS+to))` 会得到 d=0 导致循环立即退出。
waitfor() {
    local file="$1" pat="$2" timeout="${3:-15}"
    local deadline=$((SECONDS + timeout))
    while [ $SECONDS -lt $deadline ]; do
        grep -q -- "$pat" "$file" 2>/dev/null && return 0
        sleep 0.2
    done
    return 1
}

# start_serve <mock 脚本> <out> <err>:FIFO 驱动起 serve,注册 fd3,等握手。
start_serve() {
    local script="$1" out="$2" err="$3"
    local f="$ISO_CWD/ops.fifo"
    rm -f "$f"
    mkfifo "$f"
    ( REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$script" \
        "$REFLECT_BIN" serve <"$f" >"$out" 2>"$err" & )
    SERVE_PID=$!
    exec 3>"$f"
    waitfor "$out" '"session_configured"' 20 || { echo "serve 握手超时" >&2; return 1; }
}

# stop_serve:发 shutdown,等 shutdown_complete,关 fd3。
stop_serve() {
    printf '%s\n' '{"id":"sh","op":{"type":"shutdown"}}' >&3
    waitfor "$SERVE_OUT" '"shutdown_complete"' 15 || true
    exec 3>&-
    wait "$SERVE_PID" 2>/dev/null
}

# wop <json op>:向 serve 写一个 submission(字段 op,非 msg)。
wop() { printf '%s\n' "$1" >&3; }

# ── 写含 MCP server 段的隔离 config ──────────────────────────────────────
# write_mcp_config <config 路径> <server 段 toml>:追加 MCP 段。
write_mcp_config() {
    local cfg="$1" section="$2"
    printf '\n%s\n' "$section" >> "$cfg"
}

# ════════════════════════════════════════════════════════════════════
# 组 1: MCP —— 生命周期 + in-turn 调用
# ════════════════════════════════════════════════════════════════════
integ_mcp() {
    make_isolated_env
    trap cleanup_isolated_env RETURN
    if ! have_mock_mcp; then
        tcase "mcp 跳过(mock_mcp_server 未构建)" false
        return
    fi

    # 两个 server:fsx(真)与 bad(不存在,验 mcp_server_failed)。
    write_mcp_config "$ISO_HOME/.reflect/config.toml" "[mcp_servers.fsx]
type = \"stdio\"
command = \"$MOCK_MCP\""
    write_mcp_config "$ISO_HOME/.reflect/config.toml" "[mcp_servers.bad]
type = \"stdio\"
command = \"/nonexistent/bad_mcp_server\""

    # 模型调用 MCP 工具后收尾。
    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/m.jsonl" \
        '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"mcp-hello"}}' \
        '{"type":"text","text":"done"}'
    local out="$ISO_CWD/mcp.out" err="$ISO_CWD/mcp.err"
    SERVE_OUT="$out"
    start_serve "$s/m.jsonl" "$out" "$err" || { tcase "mcp serve 握手" false; stop_serve; return; }

    # ── 生命周期事件 ──────────────────────────────────────────────
    waitfor "$out" '"mcp_server_started"' 30 || echo NO_MCP_STARTED
    tcase "mcp_server_started 事件(fsx)" \
        jq -e 'select(.msg.type=="mcp_server_started" and .msg.server=="fsx")' "$out"
    tcase "mcp_server_started 含 tool_names(mcp__fsx__echo)" \
        jq -e 'select(.msg.type=="mcp_server_started" and (.msg.tool_names|index("mcp__fsx__echo")!=null))' "$out"
    tcase "mcp_server_started tool_count=2" \
        jq -e 'select(.msg.type=="mcp_server_started" and .msg.server=="fsx" and .msg.tool_count==2)' "$out"
    tcase "mcp_server_failed 事件(bad server)" \
        jq -e 'select(.msg.type=="mcp_server_failed" and .msg.server=="bad")' "$out"
    tcase "mcp_server_failed 含错误信息" \
        jq -e 'select(.msg.type=="mcp_server_failed" and (.msg.error|length)>0)' "$out"

    # ── in-turn 调用:等注册完成(Started 在 list_tools 后发出)再发 user_input ──
    sleep 1
    wop '{"id":"u1","op":{"type":"user_input","items":[{"type":"text","text":"call it"}]}}'
    waitfor "$out" '"mcp_tool_invoked"' 30 || echo NO_INVOKED
    tcase "mcp_tool_invoked 事件" \
        jq -e 'select(.msg.type=="mcp_tool_invoked")' "$out"
    tcase "mcp_tool_invoked server=fsx tool=echo" \
        jq -e 'select(.msg.type=="mcp_tool_invoked" and .msg.server=="fsx" and .msg.tool=="echo")' "$out"
    waitfor "$out" '"mcp-hello"' 30 || echo NO_ECHO
    tcase "tool_call_end 回传 MCP echo 输出" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("mcp-hello")))' "$out"
    tcase "MCP 调用成功 is_error=false" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$out"
    waitfor "$out" '"turn_complete"' 40 || echo NO_TC
    tcase "MCP 工具 turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    stop_serve
    tcase "mcp shutdown 正常" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ════════════════════════════════════════════════════════════════════
# 组 2: skill —— 工作区 SKILL.md + load_skill
# ════════════════════════════════════════════════════════════════════
integ_skill() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # workspace skill:frontmatter name/description 必填,tools 声明激活列表。
    mkdir -p "$ISO_CWD/.reflect/skills/greet"
    cat > "$ISO_CWD/.reflect/skills/greet/SKILL.md" <<'EOF'
---
name: greet
description: Greets people politely
tools: [echo]
triggers: [hello]
---
# Greet
GREET_BODY_MARKER: always say hello.
EOF

    # 模型调用 load_skill 激活,再收尾。
    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/s.jsonl" \
        '{"type":"tool_call","name":"load_skill","args":{"name":"greet"}}' \
        '{"type":"text","text":"done"}'
    local out="$ISO_CWD/skill.out" err="$ISO_CWD/skill.err"
    ( REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/s.jsonl" \
        "$REFLECT_BIN" exec "use the greet skill" >"$out" 2>"$err" )
    local rc=$?
    tcase "load_skill exec 正常退出" test "$rc" -eq 0
    tcase "tool_call_begin load_skill" \
        jq -e 'select(.msg.type=="tool_call_begin" and .msg.tool_name=="load_skill")' "$out"
    tcase "load_skill 输出含 skill 正文" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("GREET_BODY_MARKER")))' "$out"
    tcase "load_skill 输出含激活工具 echo" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("echo")))' "$out"
    tcase "load_skill 调用成功 is_error=false" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$out"
    tcase "load_skill 后 turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    # 不存在的 skill → 优雅报错(skill not found),turn 不崩。
    make_mock_script "$s/s2.jsonl" \
        '{"type":"tool_call","name":"load_skill","args":{"name":"nope"}}' \
        '{"type":"text","text":"done"}'
    local out2="$ISO_CWD/skill2.out"
    ( REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/s2.jsonl" \
        "$REFLECT_BIN" exec "use nope" >"$out2" 2>/dev/null )
    tcase "load_skill 不存在的 skill 优雅报错" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==true and (.msg.output|tostring|contains("not found")))' "$out2"
}

# ════════════════════════════════════════════════════════════════════
# 组 3: notes —— add_session_note 落盘
# ════════════════════════════════════════════════════════════════════
integ_notes() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/n.jsonl" \
        '{"type":"tool_call","name":"add_session_note","args":{"text":"note-marker-xyz"}}' \
        '{"type":"text","text":"done"}'
    local out="$ISO_CWD/notes.out"
    ( REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/n.jsonl" \
        "$REFLECT_BIN" exec "note it" >"$out" 2>/dev/null )
    tcase "add_session_note 调用成功" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$out"

    # 落盘路径:$HOME/.reflect/session-notes/<thread_id>.jsonl(REFLECT_HOME 未设)。
    local notes_dir="$ISO_HOME/.reflect/session-notes"
    tcase "session-notes 目录建立" test -d "$notes_dir"
    local any
    any=$(ls "$notes_dir"/*.jsonl 2>/dev/null | head -1)
    tcase "session-notes JSONL 落盘" test -n "$any" -a -s "$any"
    tcase "notes 内容含 marker" \
        grep -rq "note-marker-xyz" "$notes_dir"
    tcase "notes 行是合法 JSON(含 created_at)" \
        bash -c "jq -e '.created_at' '$notes_dir'/*.jsonl"
}

# ════════════════════════════════════════════════════════════════════
# 组 4: bubble —— 非阻塞通知 + 自动放行
# ════════════════════════════════════════════════════════════════════
# 用 MCP 工具(Prompt 权限)触发:Bubble 模式下 gate 发 permission_bubble
# 后自动放行执行;Auto 权限的内置工具(如 echo)不走 gate,不会发 bubble。
# 注意:exec/serve 默认 `approvals=false`(gate 关闭,JSONL 免确认语义),
# 需 REFLECT_APPROVALS=1 显式开启审批链路,gate 才会走到 bubble 分支。
integ_bubble() {
    make_isolated_env
    trap cleanup_isolated_env RETURN
    if ! have_mock_mcp; then
        tcase "bubble 跳过(mock_mcp_server 未构建)" false
        return
    fi
    export REFLECT_APPROVALS=1
    trap 'unset REFLECT_APPROVALS; cleanup_isolated_env' RETURN

    write_mcp_config "$ISO_HOME/.reflect/config.toml" "[mcp_servers.fsx]
type = \"stdio\"
command = \"$MOCK_MCP\""

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/b.jsonl" \
        '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"bubble-echo"}}' \
        '{"type":"text","text":"done"}'
    local out="$ISO_CWD/bubble.out" err="$ISO_CWD/bubble.err"
    SERVE_OUT="$out"
    start_serve "$s/b.jsonl" "$out" "$err" || { tcase "bubble serve 握手" false; stop_serve; return; }

    waitfor "$out" '"mcp_server_started"' 30 || echo NO_B_MCP
    sleep 1

    # ── 切 bubble 模式 ─────────────────────────────────────────────
    wop '{"id":"b1","op":{"type":"set_permission_mode","mode":"bubble"}}'
    waitfor "$out" '"permission_mode_changed"' 10 || echo NO_BMC
    tcase "set_permission_mode bubble 触发 permission_mode_changed" \
        jq -e 'select(.msg.type=="permission_mode_changed" and .msg.to=="bubble")' "$out"

    # ── bubble 模式下调用 Prompt 权限的 MCP 工具 ──────────────────
    wop '{"id":"b2","op":{"type":"user_input","items":[{"type":"text","text":"echo it"}]}}'
    waitfor "$out" '"permission_bubble"' 30 || echo NO_BUBBLE
    tcase "bubble 模式触发 permission_bubble 事件" \
        jq -e 'select(.msg.type=="permission_bubble")' "$out"
    tcase "permission_bubble 指向 MCP 工具" \
        jq -e 'select(.msg.type=="permission_bubble" and .msg.tool_name=="mcp__fsx__echo")' "$out"
    tcase "permission_bubble 含 risk 字段" \
        jq -e 'select(.msg.type=="permission_bubble" and (.msg.risk|type)=="string")' "$out"
    tcase "bubble 模式自动放行并执行工具" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$out"
    tcase "bubble 工具输出回传(bubble-echo)" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("bubble-echo")))' "$out"
    waitfor "$out" '"turn_complete"' 40 || echo NO_BTC
    tcase "bubble 模式 turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    stop_serve
    tcase "bubble shutdown 正常" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ════════════════════════════════════════════════════════════════════
# 组 5: hotreload —— 配置热重载
# ════════════════════════════════════════════════════════════════════
integ_hotreload() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/h.jsonl" '{"type":"text","text":"x"}'
    local out="$ISO_CWD/hot.out" err="$ISO_CWD/hot.err"
    SERVE_OUT="$out"
    start_serve "$s/h.jsonl" "$out" "$err" || { tcase "hotreload serve 握手" false; stop_serve; return; }

    # 追加 [compact] 段(debounce 250ms;watcher 有 60s 轮询兜底,
    # 正常 <3s 内触发 fs 事件)。
    sleep 0.5
    printf '\n[compact]\ntrigger_tokens = 99999\n' >> "$ISO_HOME/.reflect/config.toml"
    touch "$ISO_HOME/.reflect/config.toml"
    waitfor "$out" '"config_reloaded"' 25 || echo NO_RELOAD
    tcase "config_reloaded 事件" \
        jq -e 'select(.msg.type=="config_reloaded")' "$out"
    tcase "config_reloaded path 指向 config.toml" \
        jq -e 'select(.msg.type=="config_reloaded" and (.msg.path|endswith("config.toml")))' "$out"
    tcase "config_reloaded sections_changed 非空" \
        jq -e 'select(.msg.type=="config_reloaded" and (.msg.sections_changed|length)>0)' "$out"
    tcase "config_reloaded 含 compact 段" \
        jq -e 'select(.msg.type=="config_reloaded" and (.msg.sections_changed|index("compact")!=null))' "$out"

    # 热重载后会话存活:发一轮 user_input 正常完成。
    wop '{"id":"h1","op":{"type":"user_input","items":[{"type":"text","text":"still alive"}]}}'
    waitfor "$out" '"turn_complete"' 30 || echo NO_HTC
    tcase "热重载后会话存活(user_input turn 完成)" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    stop_serve
    tcase "hotreload shutdown 正常" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ════════════════════════════════════════════════════════════════════
# 组 6: plugin —— marketplace → install → enable → plugin_loaded
# ════════════════════════════════════════════════════════════════════
integ_plugin() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 本地 directory marketplace(cli_deep.sh 同构,owner 字段必填)。
    local mkt="$ISO_CWD/mkt"
    mkdir -p "$mkt/.claude-plugin" "$mkt/plugins/hello"
    cat > "$mkt/plugins/hello/plugin.toml" <<'EOF'
name = "hello"
version = "0.1.0"
description = "integ test plugin"
EOF
    cat > "$mkt/.claude-plugin/marketplace.json" <<EOF
{
  "name": "integ-mk",
  "owner": { "name": "Test" },
  "plugins": [
    { "name": "hello", "version": "0.1.0", "source": { "source": "directory", "path": "./plugins/hello" } }
  ]
}
EOF
    "$REFLECT_BIN" plugin marketplace add integ-mk --from directory --path "$mkt" >"$ISO_CWD/p1.out" 2>&1
    tcase "plugin marketplace add 成功" test $? -eq 0
    "$REFLECT_BIN" plugin install "hello@integ-mk" >"$ISO_CWD/p2.out" 2>&1
    tcase "plugin install 成功" test $? -eq 0
    "$REFLECT_BIN" plugin enable "hello@integ-mk" >"$ISO_CWD/p3.out" 2>&1
    tcase "plugin enable 成功" test $? -eq 0

    # exec 启动期 bootstrap_plugins 同步 enabled 列表 → plugin_loaded 事件。
    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/pl.jsonl" '{"type":"text","text":"hi"}'
    local out="$ISO_CWD/plugin.out"
    ( REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/pl.jsonl" \
        "$REFLECT_BIN" exec "hi" >"$out" 2>/dev/null )
    tcase "plugin_loaded 事件" \
        jq -e 'select(.msg.type=="plugin_loaded")' "$out"
    tcase "plugin_loaded plugin=hello@integ-mk" \
        jq -e 'select(.msg.type=="plugin_loaded" and .msg.plugin=="hello@integ-mk")' "$out"
    tcase "plugin_loaded 含 version" \
        jq -e 'select(.msg.type=="plugin_loaded" and .msg.version=="0.1.0")' "$out"
    tcase "plugin_loaded 后 turn 正常完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
integ_mcp
integ_skill
integ_notes
integ_bubble
integ_hotreload
integ_plugin

tfinish "integrations.sh" || exit 1
exit 0
