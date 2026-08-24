#!/usr/bin/env bash
# tests/rust/cli_core.sh —— 核心 CLI 子命令端到端(隔离 HOME + mock provider)。
#
# 覆盖:exec(单轮/工具调用/plan-mode/resume 三选一/参数互斥)、
#       serve(握手/远程工具/interrupt/优雅退出)、session 全生命周期、
#       traces、login、config、doctor、version、update、--cwd 全局旗标、
#       --help 子命令清单。
#
# 用法:由 tests/rust/run.sh source;也可独立执行。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary

# ════════════════════════════════════════════════════════════════════════
# 组 1:顶层信息类
# ════════════════════════════════════════════════════════════════════════
cli_core_info() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    tcase "version 打印版本号" "$REFLECT_BIN" version
    tcase "--version 打印版本号" "$REFLECT_BIN" --version
    tcase "update 打印升级说明" "$REFLECT_BIN" update
    tcase "doctor 干净配置下退出 0" "$REFLECT_BIN" doctor

    # --help 应列出全部 17 个子命令(防意外删改命令)
    "$REFLECT_BIN" --help > help.txt 2>&1
    local sub
    for sub in exec serve discussion login mcp config session traces doctor \
        plugin lsp task pipeline security workspace update version; do
        tcase "--help 含子命令 $sub" grep -qE "^  $sub" help.txt
    done

    # --cwd 全局旗标:对任意子命令生效
    tcase "--cwd 全局旗标生效" "$REFLECT_BIN" --cwd "$ISO_CWD" session ls
}

# ════════════════════════════════════════════════════════════════════════
# 组 2:exec —— headless 单轮
# ════════════════════════════════════════════════════════════════════════
cli_core_exec() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 2a. 默认 mock 回复 + 完整事件流
    REFLECT_MODEL=mock "$REFLECT_BIN" exec "你好" > exec1.jsonl 2>exec1.err
    tcase "exec mock 单轮退出 0" test $? -eq 0
    tcase "exec 发出 session_configured(provider=mock)" \
        jq -e 'select(.msg.type=="session_configured" and .msg.provider=="mock")' exec1.jsonl
    tcase "exec 发出 turn_started" \
        jq -e 'select(.msg.type=="turn_started")' exec1.jsonl
    tcase "exec 发出 agent_message_delta" \
        jq -e 'select(.msg.type=="agent_message_delta")' exec1.jsonl
    tcase "exec 发出 token_count" \
        jq -e 'select(.msg.type=="token_count")' exec1.jsonl
    tcase "exec 以 turn_complete(success)收尾" \
        jq -e 'select(.msg.type=="turn_complete" and .msg.status=="success")' exec1.jsonl

    # 2b. mock 脚本驱动的确定性文本
    make_mock_script script1.jsonl \
        '{"type":"text","text":"e2e-first-reply"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/script1.jsonl" \
        "$REFLECT_BIN" exec "第一轮" > exec2.jsonl 2>/dev/null
    # mock 客户端会把文本拆成多个 delta(约 8 字符/块),拼接后整体比对
    tcase "exec mock 脚本文本逐字回流" \
        sh -c "jq -r 'select(.msg.type==\"agent_message_delta\").msg.delta' exec2.jsonl | tr -d '\n' | grep -qF 'e2e-first-reply'"

    # 2c. mock 脚本发起工具调用(echo 内置工具)
    make_mock_script script2.jsonl \
        '{"type":"tool_call","name":"echo","args":{"text":"工具入参回显"}}' \
        '{"type":"text","text":"tool done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/script2.jsonl" \
        "$REFLECT_BIN" exec "调工具" > exec3.jsonl 2>/dev/null
    tcase "exec 工具调用:tool_call_begin" \
        jq -e 'select(.msg.type=="tool_call_begin" and .msg.tool_name=="echo")' exec3.jsonl
    tcase "exec 工具调用:tool_call_end 成功且输出回显" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false and (.msg.output.content[0].text=="工具入参回显"))' exec3.jsonl
    tcase "exec 工具调用后 turn_complete" \
        jq -e 'select(.msg.type=="turn_complete")' exec3.jsonl

    # 2d. plan-mode 旗标(mock 下文本回复,不触发写工具)
    tcase "exec --plan-mode 退出 0" \
        env REFLECT_MODEL=mock "$REFLECT_BIN" exec --plan-mode "计划一下"

    # 2e. resume 三选一互斥(clap 层拒绝)
    "$REFLECT_BIN" exec --resume some-uuid -c "x" >/dev/null 2>&1
    tcase "exec --resume 与 -c 互斥被拒绝" test $? -ne 0
    "$REFLECT_BIN" exec -c -r 1 "x" >/dev/null 2>&1
    tcase "exec -c 与 -r 互斥被拒绝" test $? -ne 0

    # 2f. 缺 prompt 报错(usage 错误)
    "$REFLECT_BIN" exec >/dev/null 2>&1
    tcase "exec 缺 prompt 报错" test $? -ne 0

    # 2g. --auto-root / --ephemeral-tasks 旗标路径不 panic
    tcase "exec --auto-root --ephemeral-tasks 退出 0" \
        env REFLECT_MODEL=mock "$REFLECT_BIN" exec --auto-root --ephemeral-tasks "hi"
}

# ════════════════════════════════════════════════════════════════════════
# 组 3:exec resume + session 全生命周期
# ════════════════════════════════════════════════════════════════════════
cli_core_session() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 3a. 先跑一轮产生 session。show/rename/export/fork/rm 按**文件名**
    # (thread id)寻址;`session ls` 展示的是记录内部 session_id(另一个
    # UUID)—— 两个 id 都取:SID 用于操作,LSID 用于 ls 断言。
    REFLECT_MODEL=mock "$REFLECT_BIN" exec "生成会话" >/dev/null 2>&1
    SFILE=$(find "$HOME/.reflect/sessions" -name "*.jsonl" -type f | head -1)
    SID=$(basename "$SFILE" .jsonl)
    LSID=$(head -1 "$SFILE" | jq -r '.session_id // empty')
    [ -n "$SID" ] && echo "  [info] session=$SID (ls 展示 $LSID)"

    tcase "session ls 列出该会话" sh -c "\"$REFLECT_BIN\" session ls | grep -q '$LSID'"

    "$REFLECT_BIN" session show "$SID" > show.txt 2>&1
    tcase "session show 打印元数据" assert_contains x "Session:" show.txt
    tcase "session show 打印模型" assert_contains x "mock/mock-1" show.txt

    "$REFLECT_BIN" session rename "$SID" "e2e-改名" > ren.txt 2>&1
    tcase "session rename 报告改名成功" grep -q "Renamed session" ren.txt

    "$REFLECT_BIN" session export "$SID" > export.md 2>&1
    tcase "session export 输出 markdown 头" assert_contains x "# Reflect Session" export.md
    tcase "session export --out 写文件" \
        sh -c "\"$REFLECT_BIN\" session export '$SID' --out exp2.md && test -s exp2.md"

    # 3b. fork + 续跑子会话
    FORK_OUT=$("$REFLECT_BIN" session fork "$SID" 2>&1)
    CHILD=$(echo "$FORK_OUT" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | tail -1)
    tcase "session fork 产生子会话 id" test -n "$CHILD"
    tcase "session fork 子文件落盘" \
        test -n "$(find "$HOME/.reflect/sessions" -name "$CHILD.jsonl")"
    REFLECT_MODEL=mock "$REFLECT_BIN" exec --resume "$CHILD" > resume1.jsonl 2>/dev/null
    tcase "exec --resume 子会话 turn_complete" \
        jq -e 'select(.msg.type=="turn_complete")' resume1.jsonl

    # 3c. -c 续最近 / -r 按序号
    REFLECT_MODEL=mock "$REFLECT_BIN" exec -c "继续" > resume2.jsonl 2>/dev/null
    tcase "exec -c 续最近会话 turn_complete" \
        jq -e 'select(.msg.type=="turn_complete")' resume2.jsonl
    REFLECT_MODEL=mock "$REFLECT_BIN" exec -r 1 "按序号续" > resume3.jsonl 2>/dev/null
    tcase "exec -r 1 按序号续跑 turn_complete" \
        jq -e 'select(.msg.type=="turn_complete")' resume3.jsonl

    # 3d. rm 删除(--yes 跳确认)
    "$REFLECT_BIN" session rm "$CHILD" --yes >/dev/null 2>&1
    tcase "session rm --yes 删除子会话文件" \
        test -z "$(find "$HOME/.reflect/sessions" -name "$CHILD.jsonl")"
}

# ════════════════════════════════════════════════════════════════════════
# 组 4:traces
# ════════════════════════════════════════════════════════════════════════
cli_core_traces() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    REFLECT_MODEL=mock "$REFLECT_BIN" exec "生成 trace" >/dev/null 2>&1
    TFILE=$(ls "$HOME/.reflect/traces/model-io"/*.jsonl 2>/dev/null | head -1)
    tcase "exec 落盘 model-io trace 文件" test -n "$TFILE"
    TID=$(basename "$TFILE" | sed 's/^model-io-sess_//;s/\.jsonl$//')

    "$REFLECT_BIN" traces ls > tls.txt 2>&1
    tcase "traces ls 列出 trace 会话" grep -q "$TID" tls.txt
    "$REFLECT_BIN" traces show "$TID" > tshow.txt 2>&1
    tcase "traces show 打印调用详情" grep -q "main_turn" tshow.txt
    tcase "traces show 含请求/响应体" grep -q "request:" tshow.txt
}

# ════════════════════════════════════════════════════════════════════════
# 组 5:login + config
# ════════════════════════════════════════════════════════════════════════
cli_core_login_config() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 5a. login 写盘
    "$REFLECT_BIN" login --provider openai --api-key sk-e2e-openai-1234567890 --force >/dev/null 2>&1
    tcase "login 写出 config.toml" test -f "$HOME/.reflect/config.toml"
    tcase "login api_key 落盘" grep -q "sk-e2e-openai-1234567890" "$HOME/.reflect/config.toml"

    # 5b. config show 脱敏
    "$REFLECT_BIN" config show > cshow.txt 2>&1
    tcase "config show 不泄露完整 key" sh -c "! grep -q 'sk-e2e-openai-1234567890' cshow.txt"
    tcase "config show 脱敏展示尾部" grep -q "sk-…890" cshow.txt

    # 5c. config set / ls / unset
    "$REFLECT_BIN" config set anthropic.model claude-3-haiku >/dev/null 2>&1
    tcase "config set 写入值" grep -q 'claude-3-haiku' "$HOME/.reflect/config.toml"
    "$REFLECT_BIN" config ls > cls.txt 2>&1
    tcase "config ls 显示已设值" grep -q "claude-3-haiku" cls.txt
    "$REFLECT_BIN" config unset anthropic.model >/dev/null 2>&1
    tcase "config unset 清除值" sh -c "! grep -q claude-3-haiku '$HOME/.reflect/config.toml'"

    # 5d. config edit 用 EDITOR 非交互写入
    printf '#!/bin/sh\necho "[edited]" >> "$1"\n' > fake_editor.sh
    chmod +x fake_editor.sh
    tcase "config edit 调用 $EDITOR" \
        env EDITOR="$ISO_CWD/fake_editor.sh" "$REFLECT_BIN" config edit
    tcase "config edit 写入生效" grep -q "edited" "$HOME/.reflect/config.toml"
}

# ════════════════════════════════════════════════════════════════════════
# 组 6:serve(stdio JSONL 协议:握手/远程工具/interrupt/优雅退出)
# ════════════════════════════════════════════════════════════════════════
cli_core_serve() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    make_mock_script serve_script.jsonl \
        '{"type":"text","text":"serve-reply"}'
    FIFO=serve_in.fifo
    OUT=serve_out.jsonl
    mkfifo "$FIFO"

    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/serve_script.jsonl" \
        "$REFLECT_BIN" serve <"$FIFO" >"$OUT" 2>serve.err &
    local spid=$!
    exec 3>"$FIFO"

    # 6a. 握手 + 单轮
    echo '{"id":"t1","op":{"type":"user_input","items":[{"type":"text","text":"hi"}]}}' >&3
    # 6b. interrupt 打断当前 turn
    echo '{"id":"i1","op":{"type":"interrupt"}}' >&3
    # 6c. 优雅退出
    echo '{"id":"bye","op":{"type":"shutdown"}}' >&3
    exec 3>&-
    wait "$spid"
    tcase "serve 处理 interrupt+shutdown 后退出 0" test $? -eq 0
    tcase "serve 握手 session_configured" \
        jq -e 'select(.msg.type=="session_configured")' "$OUT"
    tcase "serve interrupt 触发 turn_aborted" \
        jq -e 'select(.msg.type=="turn_aborted")' "$OUT"
    tcase "serve shutdown_complete 收尾" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$OUT"

    # 6d. 远程工具全链路:注册 → LLM 调用 → 客户端执行 → 回执 → 完成
    make_mock_script tool_script.jsonl \
        '{"type":"tool_call","name":"e2e_echo","args":{"q":"ping"}}' \
        '{"type":"text","text":"tool-done"}'
    FIFO2=serve_in2.fifo
    OUT2=serve_out2.jsonl
    mkfifo "$FIFO2"
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/tool_script.jsonl" \
        "$REFLECT_BIN" serve <"$FIFO2" >"$OUT2" 2>serve2.err &
    spid=$!
    exec 4>"$FIFO2"
    echo '{"id":"reg","op":{"type":"register_tools","tools":[{"name":"e2e_echo","description":"e2e echo","parameters":{"type":"object"}}]}}' >&4
    echo '{"id":"t1","op":{"type":"user_input","items":[{"type":"text","text":"调工具"}]}}' >&4

    # 轮询等远程工具请求(最长 15s),读到 call_id 后回执
    local deadline=$((SECONDS + 15)) call_id=""
    while [ $SECONDS -lt $deadline ]; do
        if grep -q '"tool_execution_request"' "$OUT2" 2>/dev/null; then
            call_id=$(jq -r 'select(.msg.type=="tool_execution_request") | .msg.call_id' "$OUT2" | head -1)
            break
        fi
        sleep 0.2
    done
    tcase "serve 下发远程工具请求 tool_execution_request" test -n "$call_id"
    if [ -n "$call_id" ]; then
        printf '{"id":"resp","op":{"type":"tool_execution_response","call_id":"%s","output":{"content":[{"type":"text","text":"echo-back"}],"is_error":false,"metadata":{},"elapsed_ms":1}}}\n' \
            "$call_id" >&4
    fi
    # 等 turn 完成
    deadline=$((SECONDS + 15))
    while [ $SECONDS -lt $deadline ]; do
        grep -q '"turn_complete"' "$OUT2" 2>/dev/null && break
        sleep 0.2
    done
    echo '{"id":"bye","op":{"type":"shutdown"}}' >&4
    exec 4>&-
    wait "$spid" 2>/dev/null
    tcase "serve 远程工具:tool_call_end 成功" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$OUT2"
    tcase "serve 远程工具:输出含客户端回执文本" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("echo-back")))' "$OUT2"
    tcase "serve 远程工具:turn_complete 收尾" \
        jq -e 'select(.msg.type=="turn_complete")' "$OUT2"

    # 6e. serve 旗标路径不 panic
    tcase "serve --ephemeral-tasks --ephemeral-teams 空 stdin 退出 0" \
        sh -c "REFLECT_MODEL=mock \"$REFLECT_BIN\" serve --ephemeral-tasks --ephemeral-teams </dev/null"
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
cli_core_info
cli_core_exec
cli_core_session
cli_core_traces
cli_core_login_config
cli_core_serve

tfinish "cli_core.sh" || exit 1
exit 0
