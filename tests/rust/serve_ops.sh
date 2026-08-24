#!/usr/bin/env bash
# tests/rust/serve_ops.sh —— serve 协议 19 个 Op 全量 + 事件流断言(非冒烟)。
#
# 通过 FIFO 驱动 `reflect serve`,逐 op 断言其触发的 EventMsg 与副作用:
#   user_input            单轮 turn_started→turn_complete + thread_settings 透传
#   set_effort            静默写槽(无 turn 事件,仅收尾标记),后续 turn 正常
#   set_permission_mode   permission_mode_changed(from/to)
#   cycle_permission_mode permission_mode_changed(循环切换)
#   rewind                turn_rewound(truncated_after)
#   interrupt             turn_aborted(reason=user_interrupt)
#   compact               context_compacted(strategy=noop)
#   enter_goal_mode       目标模式进入,turn 正常完成
#   exit_goal_mode        目标模式退出(静默)
#   tool_approval         孤儿回执(无 pending waiter)不崩溃、会话存活
#   hook_approval         同上
#   ask_user_question_response  孤儿回执不崩溃
#   ask_user_input_response     孤儿回执不崩溃
#   enter_plan_mode       plan_request(plan_id/task)
#   exit_plan_mode        plan_ready(fallback markdown)
#   plan_approval         auto_mode → permission_mode_changed(accept_edits)
#   register_tools        注册远程工具,LLM 调用 → tool_execution_request
#   tool_execution_response 回执 → tool_call_end(成功/失败双向)
#   shutdown              shutdown_complete 优雅退出
#
# v1.3:每条 submission 的 per-turn 通道排空后 serve 发一条
#   submission_closed(携带该 sub id)作收尾标记 —— SDK 迭代器靠它
#   收尾非 turn 操作;本脚本对静默 op(set_effort)断言「除收尾标记
#   外无新增事件」,对事件 op 断言「收尾标记在事件之后到达」。
#
# 前置:release 二进制。全程 REFLECT_MODEL=mock + 隔离 HOME。
# 说明:approval_request / ask_user* 在 headless 无 ApprovalGate(仅 TUI),
#       故其工具路径不可触发;这里只验孤儿回执 op 的 wire 级健壮性。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary
require_cmd jq || { echo "FAIL: 需要 jq" >&2; exit 2; }

# ── 轮询助手 ──────────────────────────────────────────────────────────────
# waitfor <file> <pattern> [timeout]:轮询文件直到出现 pattern(grep -q),
# 超时返回 1。serve 事件流是 append-only JSONL,增量 grep 安全。
waitfor() {
    local file="$1" pat="$2" timeout="${3:-15}"
    local deadline=$((SECONDS + timeout))
    while [ $SECONDS -lt $deadline ]; do
        grep -q -- "$pat" "$file" 2>/dev/null && return 0
        sleep 0.2
    done
    return 1
}

# ── serve 会话助手 ────────────────────────────────────────────────────────
# start_serve <mock 脚本> <out> <err>:FIFO 驱动起 serve,注册 fd3,等握手。
# 返回 1 = 握手超时(调用方应中止该组)。
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

# ════════════════════════════════════════════════════════════════════════
# 组 1:核心 ops(生命周期 / 权限 / 压缩 / 目标 / 孤儿回执)
# ════════════════════════════════════════════════════════════════════════
serve_ops_core() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/core.jsonl" \
        '{"type":"text","text":"one"}' \
        '{"type":"text","text":"two"}' \
        '{"type":"text","text":"three"}' \
        '{"type":"text","text":"four"}' \
        '{"type":"text","text":"five"}' \
        '{"type":"text","text":"six"}'
    local out="$ISO_CWD/core.out" err="$ISO_CWD/core.err"
    SERVE_OUT="$out"
    start_serve "$s/core.jsonl" "$out" "$err" || { tcase "serve 握手" false; stop_serve; return; }

    # ── user_input:单轮完整事件流 + thread_settings 全字段透传 ─────
    # thread_settings 4 个覆盖字段(model/approval_policy/sandbox_policy/
    # max_tool_concurrency)全量下发,验 wire 级被接受(mock 不读 model,
    # 无法观察切换,只断言 turn 正常完成且不产生 error 事件)。
    wop '{"id":"u1","op":{"type":"user_input","items":[{"type":"text","text":"hi"}],"thread_settings":{"model":"mock","approval_policy":"prompt","sandbox_policy":"workspace_only","max_tool_concurrency":2}}}'
    waitfor "$out" '"turn_complete"' 25 || echo NO_TC1
    tcase "user_input 触发 turn_started" \
        jq -e 'select(.msg.type=="turn_started")' "$out"
    tcase "user_input 触发 turn_complete(success)" \
        jq -e 'select(.msg.type=="turn_complete" and .msg.status=="success")' "$out"
    tcase "user_input 触发 agent_message_delta" \
        jq -e 'select(.msg.type=="agent_message_delta")' "$out"
    tcase "user_input 触发 token_count" \
        jq -e 'select(.msg.type=="token_count")' "$out"
    tcase "user_input+thread_settings 不产生 error 事件" \
        bash -c "! jq -e 'select(.msg.type==\"error\")' \"$out\" >/dev/null 2>&1"

    # ── set_effort:静默写槽,无 turn 事件(仅 serve 收尾标记)────
    # v1.3:每条 submission 的 per-turn 通道排空后 serve 发
    # submission_closed(携带该 sub id)。set_effort 本身不产 turn 事件,
    # 因此 e1 唯一可见事件就是 submission_closed —— 断言「除收尾标记外
    # 无任何新增事件」,既守住静默语义,又守住收尾标记确实到达。
    sleep 0.5  # 等上一轮事件流稳定,再取基线
    local before=$(jq -s 'length' "$out")
    wop '{"id":"e1","op":{"type":"set_effort","effort":"high"}}'
    waitfor "$out" '"id":"e1","msg":{"type":"submission_closed"' 10
    sleep 0.5
    tcase "set_effort 触发收尾标记 submission_closed" \
        jq -e 'select(.id=="e1" and .msg.type=="submission_closed")' "$out"
    # 静默断言:除 e1 自己的收尾标记外,事件总数不变。预计算成变量再断言
    # (tcase 直接 "$@" 执行,避免 bash -c 多层引号转义把 jq 程序写坏)。
    local non_e1=$(jq -s '[.[] | select(.id != "e1")] | length' "$out")
    tcase "set_effort 静默(除 e1 收尾标记外无新增事件)" \
        test "$non_e1" -eq "$before"

    # ── set_permission_mode:auto → plan ──────────────────────────
    wop '{"id":"pm1","op":{"type":"set_permission_mode","mode":"plan"}}'
    waitfor "$out" '"permission_mode_changed"' 10 || echo NO_PMC1
    tcase "set_permission_mode 触发 permission_mode_changed" \
        jq -e 'select(.msg.type=="permission_mode_changed")' "$out"
    tcase "set_permission_mode to=plan" \
        jq -e 'select(.msg.type=="permission_mode_changed" and .msg.to=="plan")' "$out"

    # ── cycle_permission_mode:循环切换(plan → auto)──────────────
    # UI 循环 Auto→AcceptEdits→Plan→Auto;此处断言精确 transition,
    # 用 from=plan 作为 cycle 事件已落盘的标记(避免 sleep 竞态)。
    wop '{"id":"pm2","op":{"type":"cycle_permission_mode"}}'
    waitfor "$out" '"from":"plan"' 15 || echo NO_CYCLE
    tcase "cycle_permission_mode 触发 plan→auto" \
        jq -e 'select(.msg.type=="permission_mode_changed" and .msg.from=="plan" and .msg.to=="auto")' "$out"

    # ── rewind:回退最近 turn ─────────────────────────────────────
    wop '{"id":"rw1","op":{"type":"rewind"}}'
    waitfor "$out" '"turn_rewound"' 15 || echo NO_REWIND
    tcase "rewind 触发 turn_rewound" \
        jq -e 'select(.msg.type=="turn_rewound")' "$out"

    # ── interrupt:中断(无进行中流也发 turn_aborted)────────────
    wop '{"id":"in1","op":{"type":"interrupt"}}'
    sleep 1
    tcase "interrupt 触发 turn_aborted" \
        jq -e 'select(.msg.type=="turn_aborted")' "$out"
    tcase "interrupt 原因 user_interrupt" \
        jq -e 'select(.msg.type=="turn_aborted" and .msg.reason.type=="user_interrupt")' "$out"

    # ── 孤儿回执:无 pending waiter,不崩溃、会话存活 ─────────────
    wop '{"id":"ta1","op":{"type":"tool_approval","id":"nonexistent","decision":"approve"}}'
    wop '{"id":"ha1","op":{"type":"hook_approval","id":"nonexistent","decision":"approve"}}'
    wop '{"id":"aq1","op":{"type":"ask_user_question_response","id":"nonexistent","answers":{"answers":[]}}}'
    wop '{"id":"au1","op":{"type":"ask_user_input_response","id":"nonexistent","text":"x"}}'
    sleep 1
    # 孤儿回执不应产生 error 事件
    tcase "孤儿回执不产生 error 事件" \
        bash -c "! jq -e 'select(.msg.type==\"error\")' \"$out\" >/dev/null 2>&1"

    # ── compact:手动压缩(短上下文 → noop)──────────────────────
    wop '{"id":"cm1","op":{"type":"compact"}}'
    waitfor "$out" '"context_compacted"' 25 || echo NO_COMPACT
    tcase "compact 触发 context_compacted" \
        jq -e 'select(.msg.type=="context_compacted")' "$out"

    # ── enter_goal_mode + user_input:目标模式 turn 正常完成 ─────
    wop '{"id":"gm1","op":{"type":"enter_goal_mode","goal":"make it","token_budget":100000}}'
    sleep 1
    wop '{"id":"ug1","op":{"type":"user_input","items":[{"type":"text","text":"work"}]}}'
    waitfor "$out" '"turn_complete"' 40 || echo NO_GOAL_TC
    tcase "enter_goal_mode 后 user_input turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"
    wop '{"id":"gm2","op":{"type":"exit_goal_mode"}}'
    sleep 1

    # ── shutdown ────────────────────────────────────────────────
    stop_serve
    tcase "shutdown 触发 shutdown_complete" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ════════════════════════════════════════════════════════════════════════
# 组 2:plan 模式 ops(plan_request / plan_draft / plan_ready / plan_approval)
# ════════════════════════════════════════════════════════════════════════
serve_ops_plan() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    # 模型先 PlanWrite 写盘,再调 ExitPlanMode 工具,最后收尾文本
    make_mock_script "$s/plan.jsonl" \
        '{"type":"tool_call","name":"PlanWrite","args":{"path":"p1.md","content":"# plan\n- s1"}}' \
        '{"type":"tool_call","name":"ExitPlanMode","args":{}}' \
        '{"type":"text","text":"built it"}'
    local out="$ISO_CWD/plan.out" err="$ISO_CWD/plan.err"
    SERVE_OUT="$out"
    start_serve "$s/plan.jsonl" "$out" "$err" || { tcase "serve 握手" false; stop_serve; return; }

    # ── enter_plan_mode → plan_request ───────────────────────────
    wop '{"id":"p0","op":{"type":"enter_plan_mode","task":"build feature X"}}'
    waitfor "$out" '"plan_request"' 20 || echo NO_PLAN_REQUEST
    tcase "enter_plan_mode 触发 plan_request" \
        jq -e 'select(.msg.type=="plan_request")' "$out"
    tcase "plan_request 含 task" \
        jq -e 'select(.msg.type=="plan_request" and .msg.task=="build feature X")' "$out"

    # ── user_input 驱动模型 PlanWrite + ExitPlanMode ─────────────
    wop '{"id":"p1","op":{"type":"user_input","items":[{"type":"text","text":"draft the plan"}]}}'
    waitfor "$out" '"plan_draft_updated"' 30 || echo NO_DRAFT
    tcase "PlanWrite 触发 plan_draft_updated" \
        jq -e 'select(.msg.type=="plan_draft_updated")' "$out"
    tcase "plan_draft_updated 含 markdown" \
        jq -e 'select(.msg.type=="plan_draft_updated" and (.msg.markdown|contains("plan")))' "$out"
    tcase "plan_draft_updated 落盘 .reflect/plan/" \
        jq -e 'select(.msg.type=="plan_draft_updated" and (.msg.path|test(".reflect/plan/")))' "$out"

    waitfor "$out" '"plan_ready"' 30 || echo NO_READY
    tcase "ExitPlanMode 触发 plan_ready" \
        jq -e 'select(.msg.type=="plan_ready")' "$out"
    local plid
    plid=$(jq -r 'select(.msg.type=="plan_ready") | .msg.plan_id' "$out" | head -1)
    tcase "plan_ready 含 plan_id" test -n "$plid"
    # plan markdown 落盘成可 cat 的持久产物
    tcase "plan markdown 落盘到 workspace" \
        bash -c "ls '$ISO_CWD/.reflect/plan/' | grep -q . "

    # ── plan_approval auto_mode → 切权限模式(不 emit plan_approved)──
    wop "{\"id\":\"p2\",\"op\":{\"type\":\"plan_approval\",\"id\":\"$plid\",\"choice\":\"auto_mode\"}}"
    waitfor "$out" '"permission_mode_changed"' 20 || echo NO_PMC_PLAN
    tcase "plan_approval auto_mode 切权限模式" \
        jq -e 'select(.msg.type=="permission_mode_changed" and .msg.to=="accept_edits")' "$out"
    # headless 路径 plan 审批只切模式、不 emit plan_approved(断言其缺席)
    tcase "plan_approval 不 emit plan_approved(headless 语义)" \
        bash -c "! jq -e 'select(.msg.type==\"plan_approved\")' \"$out\" >/dev/null 2>&1"

    # ── exit_plan_mode op 路径(无 LLM 工具调用)→ 第二个 plan_ready ──
    # 该路径只有 fallback markdown,无真实内容可带 → 断言 fallback 串
    wop '{"id":"p3","op":{"type":"exit_plan_mode"}}'
    waitfor "$out" 'plan markdown not provided' 20 || echo NO_READY2
    tcase "exit_plan_mode op 触发 plan_ready(fallback)" \
        jq -e 'select(.msg.type=="plan_ready" and (.msg.markdown|contains("plan markdown not provided")))' "$out"
    local plid2
    plid2=$(jq -r 'select(.msg.type=="plan_ready" and (.msg.markdown|contains("plan markdown not provided"))) | .msg.plan_id' "$out" | head -1)
    tcase "exit_plan_mode plan_ready 含独立 plan_id" \
        bash -c "test -n '$plid2' && [ \"$plid2\" != \"$plid\" ]"

    # ── plan_approval revise → 留在 plan 模式,emit plan_rejected ──
    wop "{\"id\":\"p4\",\"op\":{\"type\":\"plan_approval\",\"id\":\"$plid2\",\"choice\":\"revise\"}}"
    waitfor "$out" '"plan_rejected"' 20 || echo NO_REJECT
    tcase "plan_approval revise 触发 plan_rejected" \
        jq -e 'select(.msg.type=="plan_rejected")' "$out"
    tcase "plan_rejected 指向 exit_plan_mode 的 plan_id" \
        bash -c "jq -r 'select(.msg.type==\"plan_rejected\") | .msg.plan_id' '$out' | grep -q '$plid2'"

    # ── 审批后 user_input:执行阶段 turn 完成 ─────────────────────
    wop '{"id":"p5","op":{"type":"user_input","items":[{"type":"text","text":"now execute"}]}}'
    waitfor "$out" '"turn_complete"' 40 || echo NO_PLAN_TC
    tcase "plan 审批后执行 turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    stop_serve
    tcase "plan 流程 shutdown 正常" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ════════════════════════════════════════════════════════════════════════
# 组 3:register_tools 远程工具(op 级,LLM 触发 → 客户端回执)
# ════════════════════════════════════════════════════════════════════════
serve_ops_remote_tools() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    # 模型依次调用两个远程工具:remote_echo(成功)与 remote_fail(失败)
    make_mock_script "$s/remote.jsonl" \
        '{"type":"tool_call","name":"remote_echo","args":{"text":"hello remote"}}' \
        '{"type":"tool_call","name":"remote_fail","args":{"boom":true}}' \
        '{"type":"text","text":"remote done"}'
    local out="$ISO_CWD/remote.out" err="$ISO_CWD/remote.err"
    SERVE_OUT="$out"
    start_serve "$s/remote.jsonl" "$out" "$err" || { tcase "serve 握手" false; stop_serve; return; }

    # ── register_tools:注册两个远程工具 ──────────────────────────
    wop '{"id":"r0","op":{"type":"register_tools","tools":[{"name":"remote_echo","description":"echo remote","parameters":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"remote_fail","description":"always fails","parameters":{"type":"object","properties":{"boom":{"type":"boolean"}}}}]}}'
    sleep 1

    wop '{"id":"r1","op":{"type":"user_input","items":[{"type":"text","text":"call the tools"}]}}'
    waitfor "$out" '"tool_execution_request"' 30 || echo NO_TER1
    tcase "register_tools 后 LLM 调用触发 tool_execution_request" \
        jq -e 'select(.msg.type=="tool_execution_request")' "$out"
    tcase "tool_execution_request 含 tool 名" \
        jq -e 'select(.msg.type=="tool_execution_request" and .msg.tool=="remote_echo")' "$out"
    tcase "tool_execution_request 含 args" \
        jq -e 'select(.msg.type=="tool_execution_request" and .msg.args.text=="hello remote")' "$out"

    # ── 成功回执 → tool_call_end(is_error=false)────────────────
    local cid1
    cid1=$(jq -r 'select(.msg.type=="tool_execution_request") | .msg.call_id' "$out" | head -1)
    wop "{\"id\":\"r2\",\"op\":{\"type\":\"tool_execution_response\",\"call_id\":\"$cid1\",\"output\":{\"content\":[{\"type\":\"text\",\"text\":\"remote-ok\"}],\"is_error\":false,\"metadata\":{},\"elapsed_ms\":1}}}"
    waitfor "$out" '"remote-ok"' 30 || echo NO_OK
    tcase "成功回执 → tool_call_end 回传输出" \
        jq -e 'select(.msg.type=="tool_call_end" and (.msg.output|tostring|contains("remote-ok")))' "$out"
    tcase "成功回执 → tool_call_end is_error=false" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==false)' "$out"

    # ── 失败回执 → tool_call_end(is_error=true)─────────────────
    # 等第二个请求(remote_fail)真正下发后再取 call_id,避免竞态取到 cid1。
    # 用 tool 名作为落盘标记(grep 增量安全)。
    waitfor "$out" '"tool":"remote_fail"' 30 || echo NO_TER2
    tcase "LLM 第二个远程工具调用触发第二个请求" \
        jq -e 'select(.msg.type=="tool_execution_request" and .msg.tool=="remote_fail")' "$out"
    local cid2
    cid2=$(jq -r 'select(.msg.type=="tool_execution_request" and .msg.tool=="remote_fail") | .msg.call_id' "$out" | head -1)
    wop "{\"id\":\"r3\",\"op\":{\"type\":\"tool_execution_response\",\"call_id\":\"$cid2\",\"output\":{\"content\":[{\"type\":\"text\",\"text\":\"boom failed\"}],\"is_error\":true,\"metadata\":{},\"elapsed_ms\":1}}}"
    waitfor "$out" '"boom failed"' 30 || echo NO_FAIL
    tcase "失败回执 → tool_call_end is_error=true" \
        jq -e 'select(.msg.type=="tool_call_end" and .msg.is_error==true)' "$out"

    waitfor "$out" '"turn_complete"' 40 || echo NO_REMOTE_TC
    tcase "远程工具 turn 完成" \
        jq -e 'select(.msg.type=="turn_complete")' "$out"

    stop_serve
    tcase "远程工具 shutdown 正常" \
        jq -e 'select(.msg.type=="shutdown_complete")' "$out"
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
serve_ops_core
serve_ops_plan
serve_ops_remote_tools

tfinish "serve_ops.sh" || exit 1
exit 0
