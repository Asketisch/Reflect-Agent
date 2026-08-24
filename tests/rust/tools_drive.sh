#!/usr/bin/env bash
# tests/rust/tools_drive.sh —— 内置工具全量驱动(非冒烟)。
#
# 通过 `reflect exec` + mock 脚本驱动每个内置工具,断言其真实输出与
# 文件系统副作用;并覆盖 read_before_edit hook 四态、bash OS 沙箱、
# git worktree、ast 结构化搜索、离线 web 工具的优雅报错等。
#
# 覆盖工具(exec 可驱动,共 17 个内置 + 若干运行时工具):
#   echo / bash / read / write / edit / delete_file / grep / glob
#   ast / get_context_remaining / tool_search / brief / image_view
#   notebook_edit / web_fetch / web_search
#   EnterWorktree / ExitWorktree / checkpoint / rewind
#   ask_user / request_human_input(headless 优雅报错)
# 另有:
#   - read_before_edit hook 四态:新建文件 Allow / 已存在未读 Deny /
#     读后未改 Allow / 读后 mtime 漂移 Deny
#   - bash OS 沙箱:workspace 内写成功,workspace 外写被 Seatbelt/Landlock 拒绝
#   - Plan 工具(EnterPlanMode/ExitPlanMode/PlanWrite)的 headless 工具路径
#     会阻塞等 plan_approval(需 TUI/serve),已在 serve_ops.sh 用 serve 覆盖,
#     此处不走 exec 路径以避免 hang。
#
# 前置:release 二进制 + jq + git。全程 REFLECT_MODEL=mock + 隔离 HOME。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary
require_cmd jq || { echo "FAIL: 需要 jq" >&2; exit 2; }
require_cmd git || { echo "SKIP: 无 git,跳过 worktree 相关" >&2; NO_GIT=1; }

# ── 事件断言助手 ──────────────────────────────────────────────────────────
# joined <out>:把 tool_call_end 经 call_id 关联到 tool_call_begin.tool_name,
# 输出 [{tool,is_error,output(串)}] 数组(JSON)。tool_call_end 本身无 tool 名,
# 必须靠 begin 配对才能按工具断言。
joined() {
    jq -s '
        def b: map(select(.msg.type=="tool_call_begin")|{(.msg.call_id):.msg.tool_name})|add//{};
        b as $x |
        [ .[] | select(.msg.type=="tool_call_end")
          | {tool:($x[.msg.call_id] // "??"), is_error:.msg.is_error, output:(.msg.output|tostring)} ]
    ' "$1" 2>/dev/null
}

# te_ok <out> <tool>:该工具存在一条 is_error=false 的 tool_call_end。
te_ok() {
    joined "$1" | jq -e --arg t "$2" \
        'map(select(.tool==$t and .is_error==false)) | length >= 1' 2>/dev/null
}
# te_err <out> <tool>:该工具存在一条 is_error=true 的 tool_call_end。
te_err() {
    joined "$1" | jq -e --arg t "$2" \
        'map(select(.tool==$t and .is_error==true)) | length >= 1' 2>/dev/null
}
# te_out <out> <tool> <子串>:该工具某条 tool_call_end 的输出含子串。
te_out() {
    joined "$1" | jq -e --arg t "$2" --arg s "$3" \
        'map(select(.tool==$t)) | map(select(.output|contains($s))) | length >= 1' 2>/dev/null
}

# ════════════════════════════════════════════════════════════════════════
# 组 1:文件系统 + 文本工具(write/read/edit/grep/glob/echo/delete_file)
# ════════════════════════════════════════════════════════════════════════
tools_fs() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 单 exec 串起 6 个只读/写工具;write 新建文件(first-write 放行),
    # read 后再 edit(满足 read_before_edit),再次 read 验证 edit 落盘。
    make_mock_script fs.jsonl \
        '{"type":"tool_call","name":"write","args":{"path":"hello.txt","content":"line1\nline2\nline3"}}' \
        '{"type":"tool_call","name":"read","args":{"path":"hello.txt"}}' \
        '{"type":"tool_call","name":"edit","args":{"path":"hello.txt","old_string":"line2","new_string":"line2-EDITED"}}' \
        '{"type":"tool_call","name":"read","args":{"path":"hello.txt"}}' \
        '{"type":"tool_call","name":"grep","args":{"pattern":"line3","path":"."}}' \
        '{"type":"tool_call","name":"glob","args":{"pattern":"*.txt"}}' \
        '{"type":"tool_call","name":"echo","args":{"text":"echo-marker"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/fs.jsonl" \
        "$REFLECT_BIN" exec "fs tools" > fs.out 2>fs.err
    tcase "fs:exec 退出 0" test $? -eq 0

    tcase "write 成功(wrote hello.txt)" \
        te_out fs.out write "wrote hello.txt"
    tcase "read 返回带行号内容" \
        te_out fs.out read "line1"
    tcase "edit 返回 unified diff" \
        te_out fs.out edit "line2-EDITED"
    tcase "edit 已落盘(再 read 含 EDITED)" \
        bash -c "grep -q 'line2-EDITED' '$ISO_CWD/hello.txt'"
    tcase "grep 命中 ./hello.txt:3:line3" \
        te_out fs.out grep "./hello.txt:3:line3"
    tcase "glob 命中 hello.txt" \
        bash -c "jq -s 'def b: map(select(.msg.type==\"tool_call_begin\")|{(.msg.call_id):.msg.tool_name})|add//{}; b as \$x| map(select(.msg.type==\"tool_call_end\" and \$x[.msg.call_id]==\"glob\")) | map(select(.msg.output|tostring|contains(\"hello.txt\")))|length>=1' '$ISO_CWD/fs.out'"
    tcase "echo 回显文本" \
        te_out fs.out echo "echo-marker"

    # delete_file:独立 exec(避免与上面共用同一 session 的 read 状态混淆)
    make_mock_script del.jsonl \
        '{"type":"tool_call","name":"delete_file","args":{"path":"hello.txt"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/del.jsonl" \
        "$REFLECT_BIN" exec "del" > del.out 2>del.err
    tcase "delete_file 成功" te_ok del.out delete_file
    tcase "delete_file 文件已删除" \
        bash -c "! test -f '$ISO_CWD/hello.txt'"
}

# ════════════════════════════════════════════════════════════════════════
# 组 2:read_before_edit hook 四态
# ════════════════════════════════════════════════════════════════════════
tools_hook() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 态A:新建文件 first-write 允许(文件不存在 → 合法新建)
    make_mock_script a.jsonl \
        '{"type":"tool_call","name":"write","args":{"path":"fresh.txt","content":"FRESH"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/a.jsonl" \
        "$REFLECT_BIN" exec a > a.out 2>a.err
    tcase "hook 态A 新建 write 放行" te_ok a.out write
    tcase "hook 态A 文件已创建" bash -c "test -f '$ISO_CWD/fresh.txt'"

    # 态B:已存在但未 read → Deny(用 bash 造文件绕过 hook)
    make_mock_script b.jsonl \
        '{"type":"tool_call","name":"bash","args":{"cmd":"printf EXISTING > pre.txt"}}' \
        '{"type":"tool_call","name":"edit","args":{"path":"pre.txt","old_string":"EXISTING","new_string":"X"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/b.jsonl" \
        "$REFLECT_BIN" exec b > b.out 2>b.err
    tcase "hook 态B 未读 edit 被 Deny" te_err b.out edit
    tcase "hook 态B 拒绝原因 never read" te_out b.out edit "never read"

    # 态C:read 后再 edit → Allow
    make_mock_script c.jsonl \
        '{"type":"tool_call","name":"bash","args":{"cmd":"printf C1 > c.txt"}}' \
        '{"type":"tool_call","name":"read","args":{"path":"c.txt"}}' \
        '{"type":"tool_call","name":"edit","args":{"path":"c.txt","old_string":"C1","new_string":"C2"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/c.jsonl" \
        "$REFLECT_BIN" exec c > c.out 2>c.err
    tcase "hook 态C 读后 edit 放行" te_ok c.out edit
    tcase "hook 态C 编辑已落盘" bash -c "grep -q C2 '$ISO_CWD/c.txt'"

    # 态D:read 后外部改动(mtime 漂移超容差)→ Deny
    make_mock_script d.jsonl \
        '{"type":"tool_call","name":"bash","args":{"cmd":"printf D1 > d.txt"}}' \
        '{"type":"tool_call","name":"read","args":{"path":"d.txt"}}' \
        '{"type":"tool_call","name":"bash","args":{"cmd":"sleep 1; printf D9 > d.txt"}}' \
        '{"type":"tool_call","name":"edit","args":{"path":"d.txt","old_string":"D1","new_string":"Z"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/d.jsonl" \
        "$REFLECT_BIN" exec d > d.out 2>d.err
    tcase "hook 态D 漂移 edit 被 Deny" te_err d.out edit
}

# ════════════════════════════════════════════════════════════════════════
# 组 3:bash + OS 沙箱(workspace 内写成功 / 越界写被拒)
# ════════════════════════════════════════════════════════════════════════
tools_sandbox() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # workspace 内写:应成功
    make_mock_script in.jsonl \
        '{"type":"tool_call","name":"bash","args":{"cmd":"echo INWS > inws.txt"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/in.jsonl" \
        "$REFLECT_BIN" exec in > in.out 2>in.err
    tcase "sandbox workspace 内 bash 写成功" \
        bash -c "grep -q INWS '$ISO_CWD/inws.txt'"

    # workspace 外写:Seatbelt/Landlock 应拒绝,文件不落地。
    # 用 $ROOT(仓库根,不在 workspace 且不在 /tmp|/var/folders 放行区)作越界目标。
    local esc="$ROOT/.sandbox_probe_$$_$(date +%s)"
    rm -f "$esc"
    make_mock_script out.jsonl \
        "{\"type\":\"tool_call\",\"name\":\"bash\",\"args\":{\"cmd\":\"echo ESC > $esc\"}}" \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/out.jsonl" \
        "$REFLECT_BIN" exec out > out.out 2>out.err
    tcase "sandbox 越界 bash 写被拒(is_error)" te_err out.out bash
    tcase "sandbox 越界文件未落地" bash -c "! test -f '$esc'"
    rm -f "$esc"
}

# ════════════════════════════════════════════════════════════════════════
# 组 4:ast 结构化搜索 + 信息类工具(context/tool_search/brief)
# ════════════════════════════════════════════════════════════════════════
tools_ast_info() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    printf 'fn alpha() -> i32 { 1 }\nfn beta() -> i32 { alpha() + 2 }\n' > code.rs
    make_mock_script info.jsonl \
        '{"type":"tool_call","name":"ast","args":{"action":"list_languages"}}' \
        '{"type":"tool_call","name":"ast","args":{"action":"search","path":"code.rs","pattern":"text:alpha"}}' \
        '{"type":"tool_call","name":"get_context_remaining","args":{}}' \
        '{"type":"tool_call","name":"tool_search","args":{"query":"grep"}}' \
        '{"type":"tool_call","name":"brief","args":{"title":"B","content":"brief-body"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/info.jsonl" \
        "$REFLECT_BIN" exec info > info.out 2>info.err
    tcase "ast list_languages 含 rust" te_out info.out ast "rust"
    tcase "ast search 命中 alpha" te_out info.out ast "alpha"
    tcase "get_context_remaining 报 token 用量" te_out info.out get_context_remaining "tokens"
    tcase "tool_search 命中 grep 工具" te_out info.out tool_search "grep"
    tcase "brief 渲染标题+正文" te_out info.out brief "brief-body"
}

# ════════════════════════════════════════════════════════════════════════
# 组 5:image_view + 离线 web 工具(web_fetch/web_search 优雅报错)
# ════════════════════════════════════════════════════════════════════════
tools_media_web() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 造一个最小 1x1 PNG(image_view 只需能读成 image content block)
    printf '\x89PNG\r\n\x1a\n' > img.png
    make_mock_script m.jsonl \
        '{"type":"tool_call","name":"image_view","args":{"path":"img.png"}}' \
        '{"type":"tool_call","name":"web_fetch","args":{"url":"http://127.0.0.1:1/x"}}' \
        '{"type":"tool_call","name":"web_search","args":{"query":"offline q"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/m.jsonl" \
        "$REFLECT_BIN" exec m > m.out 2>m.err
    tcase "image_view 返回 image 内容块" \
        bash -c "jq -s 'def b: map(select(.msg.type==\"tool_call_begin\")|{(.msg.call_id):.msg.tool_name})|add//{}; b as \$x| map(select(.msg.type==\"tool_call_end\" and \$x[.msg.call_id]==\"image_view\")) | map(select(.msg.output|tostring|contains(\"image/png\")))|length>=1' '$ISO_CWD/m.out'"
    # web_fetch:回环地址在受阻断范围 → 优雅 InvalidArgs(非 panic)
    tcase "web_fetch 回环地址被拒(优雅报错)" te_out m.out web_fetch "blocked range"
    # web_search:未配 BRAVE_API_KEY → 优雅报错
    tcase "web_search 缺 key 优雅报错" te_out m.out web_search "BRAVE_API_KEY"
}

# ════════════════════════════════════════════════════════════════════════
# 组 6:notebook_edit(只读 list 放行 / 写子动作未读被 hook 拦)
# ════════════════════════════════════════════════════════════════════════
tools_notebook() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    cat > nb.ipynb <<'NBEOF'
{"cells":[{"cell_type":"code","execution_count":null,"metadata":{},"outputs":[],"source":["print(1)\n"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}
NBEOF
    # list 只读(不受 hook 约束);insert 写盘但未先 read → 被 hook Deny
    make_mock_script nb.jsonl \
        '{"type":"tool_call","name":"notebook_edit","args":{"action":"list","path":"nb.ipynb"}}' \
        '{"type":"tool_call","name":"notebook_edit","args":{"action":"insert","path":"nb.ipynb","index":1,"source":"print(2)\n","cell_type":"code"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/nb.jsonl" \
        "$REFLECT_BIN" exec nb > nb.out 2>nb.err
    tcase "notebook list 返回单元格" te_out nb.out notebook_edit "code"
    tcase "notebook insert 未读被 hook Deny" te_err nb.out notebook_edit
}

# ════════════════════════════════════════════════════════════════════════
# 组 7:git worktree(EnterWorktree → ExitWorktree)
# ════════════════════════════════════════════════════════════════════════
tools_worktree() {
    if [ "${NO_GIT:-0}" = "1" ]; then
        tcase "worktree 跳过(无 git)" true
        return
    fi
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # worktree 依赖 git 仓库:先初始化并落一个 commit
    git init -q .
    git config user.email t@t; git config user.name t
    printf 'x\n' > seed.txt; git add seed.txt; git commit -qm init

    make_mock_script wt.jsonl \
        '{"type":"tool_call","name":"EnterWorktree","args":{}}' \
        '{"type":"tool_call","name":"ExitWorktree","args":{}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/wt.jsonl" \
        "$REFLECT_BIN" exec wt > wt.out 2>wt.err
    tcase "EnterWorktree 成功进入" te_out wt.out EnterWorktree "Entered worktree"
    tcase "ExitWorktree 成功退出" te_out wt.out ExitWorktree "Exited worktree"
    tcase "worktree 目录已建立" \
        bash -c "ls -d '$ISO_CWD'/.git/worktrees/reflect-wt-* >/dev/null 2>&1"
}

# ════════════════════════════════════════════════════════════════════════
# 组 8:checkpoint / rewind 工具(优雅行为)
# ════════════════════════════════════════════════════════════════════════
tools_checkpoint() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    make_mock_script ck.jsonl \
        '{"type":"tool_call","name":"checkpoint","args":{"action":"list"}}' \
        '{"type":"tool_call","name":"rewind","args":{}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/ck.jsonl" \
        "$REFLECT_BIN" exec ck > ck.out 2>ck.err
    tcase "checkpoint list 正常返回" te_ok ck.out checkpoint
    tcase "checkpoint list 提示无检查点" te_out ck.out checkpoint "checkpoint"
    # rewind 无 sha 且无当前 turn 检查点 → 优雅 InvalidArgs(非 panic)
    tcase "rewind 无参优雅报错" te_err ck.out rewind
    tcase "rewind 报错含 sha 提示" te_out ck.out rewind "sha"
}

# ════════════════════════════════════════════════════════════════════════
# 组 9:ask_user / request_human_input(headless 无 ApprovalGate 的优雅行为)
# ════════════════════════════════════════════════════════════════════════
tools_ask() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # ask_user 用错参数键 → 优雅 InvalidArgs(说明 wire 校验生效)
    # request_human_input headless 无 ApprovalGate → 优雅报错
    make_mock_script au.jsonl \
        '{"type":"tool_call","name":"ask_user","args":{"question":"q?"}}' \
        '{"type":"tool_call","name":"request_human_input","args":{"prompt":"p?"}}' \
        '{"type":"text","text":"done"}'
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$ISO_CWD/au.jsonl" \
        "$REFLECT_BIN" exec au > au.out 2>au.err
    tcase "ask_user 非法参数优雅报错" te_err au.out ask_user
    tcase "request_human_input headless 优雅报错" te_err au.out request_human_input
    tcase "headless 下 turn 仍正常收尾" \
        bash -c "jq -e 'select(.msg.type==\"turn_complete\")' '$ISO_CWD/au.out'"
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
tools_fs
tools_hook
tools_sandbox
tools_ast_info
tools_media_web
tools_notebook
tools_worktree
tools_checkpoint
tools_ask

tfinish "tools_drive.sh" || exit 1
exit 0
