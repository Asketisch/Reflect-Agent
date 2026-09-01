#!/usr/bin/env bash
# tests/rust/cli_deep.sh —— CLI 全子命令深度使用测试(非冒烟)。
#
# 在 cli_core / cli_modules 的冒烟覆盖之上,补齐各子命令的
# 完整参数路径、成功/失败双向断言:
#   login     4 provider 路径 + 缺 key 报错 + 短 key 告警 + --force + api_key 脱敏
#   config    set / unset / ls / show / --reveal / 非法 key 报错
#   mcp       stdio+http 双传输 add / 互斥报错 / ls / show / remove /
#             test(真实 mock server 成功 + 坏 binary 失败)/ registry 三路径
#   session   双 ID 前缀解析回归(文件名前缀 + 内部 id 前缀 + 歧义报错)+
#             rename / export(--out) / fork / rm(--yes / 拒绝)
#   traces    ls / show 前缀 / 不存在报错
#   task      create / ls / show / update(status+subject) / stop / purge +
#             team 全链路 + 非数字 id clap 报错
#   plugin    marketplace add / ls / refresh / remove + 从 marketplace 安装 +
#             info / show / enable / disable / uninstall 全链路
#   workspace ls / clone 本地仓库 / --depth 浅克隆 / 坏 URL 报错
#   update    常规 + --check-only
#   doctor    常规 + --check-network(mock provider DNS 失败仍 rc=0)
#   lsp       空配置提示 / 未配置 status 报错 / 配置后 list+status
#   pipeline  错误路径(team 不存在 / 自定义 label 无 runner —— 产品限制)
#   discussion mock 驱动真多 agent(sequential,transcript 非空)+ --output 文件
#
# 前置:release 二进制(缺则 run.sh 先构建);jq / git 可用。
# 全程离线:REFLECT_MODEL=mock + 隔离 HOME,不污染真实 ~/.reflect。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary
require_cmd jq || { echo "FAIL: 需要 jq" >&2; exit 2; }

# ── 公共助手 ──────────────────────────────────────────────────────────────

# write_fake_session <base> <thread_id> <internal_id>:在 rollout 目录树写一个
# 最小 2 行 JSONL(session_meta + message)。用于构造可控 UUID 的 fixture
# (真实 exec 产生的 UUID 不可控,无法构造前缀歧义 / 双 ID 场景)。
write_fake_session() {
    local base="$1" tid="$2" iid="$3"
    mkdir -p "$base/2026/08/01"
    cat > "$base/2026/08/01/$tid.jsonl" <<EOF
{"type":"session_meta","session_id":"$iid","model":"mock/mock-1","started_at":"2026-08-01T00:00:00Z","message_count":1}
{"type":"message","role":"user","content":"fake session body"}
EOF
}

# real_session_ids:执行一次 mock exec 后,输出 "THREAD INTERNAL"(文件名 id
# 与首行内部 id,双 ID 设计下通常不同)。
real_session_ids() {
    local file
    file=$(find "$ISO_HOME/.reflect/sessions" -name '*.jsonl' 2>/dev/null | head -1)
    [ -n "$file" ] || return 1
    local thread internal
    thread=$(basename "$file" .jsonl)
    internal=$(head -1 "$file" | jq -r '.session_id')
    echo "$thread $internal"
}

# ════════════════════════════════════════════════════════════════════════
# 组 1:login —— 4 provider 路径 + 校验 + 脱敏
# ════════════════════════════════════════════════════════════════════════
cli_deep_login() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 4 个 provider 名(ollama 与 local 别名各走一遍)
    "$REFLECT_BIN" login --provider anthropic --api-key sk-ant-test-12345678 > lg1.out 2>&1
    tcase "login anthropic 写入配置" test $? -eq 0
    tcase "login 提示 Config saved" grep -q "Config saved to" lg1.out
    tcase "login 后 config show 含 [anthropic] 段" \
        bash -c "'$REFLECT_BIN' config show | grep -q '\[anthropic\]'"

    "$REFLECT_BIN" login --provider openai --api-key sk-oai-test-12345678 > lg2.out 2>&1
    tcase "login openai 退出 0" test $? -eq 0

    # api_key 脱敏:必须在短 key 覆写 openai 之前检查
    "$REFLECT_BIN" config show > cfg_masked.txt 2>&1
    tcase "config show 脱敏(不含完整 key)" \
        bash -c "! grep -q 'sk-oai-test-12345678' cfg_masked.txt"
    "$REFLECT_BIN" config show --reveal > cfg_reveal.txt 2>&1
    tcase "config show --reveal 含完整 key" \
        grep -q "sk-oai-test-12345678" cfg_reveal.txt

    "$REFLECT_BIN" login --provider ollama --api-key ollama-local-key-123 > lg3.out 2>&1
    tcase "login ollama 退出 0" test $? -eq 0

    "$REFLECT_BIN" login --provider local --api-key ollama-local-key-456 > lg4.out 2>&1
    tcase "login local(ollama 别名)退出 0" test $? -eq 0

    # 缺 key 报错(flag 与 stdin 都不可用)
    "$REFLECT_BIN" login --provider openai </dev/null > lg5.out 2>&1
    tcase "login 缺 api-key 报错" test $? -ne 0
    tcase "login 报错含 api_key" grep -q "api_key" lg5.out

    # 短 key:默认告警但仍保存;--force 静默(会覆写 openai key,放最后)
    "$REFLECT_BIN" login --provider openai --api-key sk-x > lg6.out 2>&1
    tcase "login 短 key 仍退出 0" test $? -eq 0
    tcase "login 短 key 打告警" grep -q "very short" lg6.out
    "$REFLECT_BIN" login --provider openai --api-key sk-x --force > lg7.out 2>&1
    tcase "login --force 短 key 退出 0" test $? -eq 0
    tcase "login --force 不告警" bash -c "! grep -q 'very short' lg7.out"
}

# ════════════════════════════════════════════════════════════════════════
# 组 2:config —— set / unset / ls / 非法 key
# ════════════════════════════════════════════════════════════════════════
cli_deep_config() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    "$REFLECT_BIN" config set anthropic.model claude-x-test > c1.out 2>&1
    tcase "config set 成功" test $? -eq 0
    tcase "config set 回显" grep -q "set anthropic.model = claude-x-test" c1.out

    "$REFLECT_BIN" config ls > c2.out 2>&1
    tcase "config ls 含新值" grep -q "anthropic.model  *= *claude-x-test" c2.out
    tcase "config ls 未设置项标 <unset>" grep -q "<unset>" c2.out

    # 非法 key:报错并列出允许清单
    "$REFLECT_BIN" config set not.a.real.key 1 > c3.out 2>&1
    tcase "config set 非法 key 报错" test $? -ne 0
    tcase "config set 报错列出允许 key" grep -q "allowed keys" c3.out
    tcase "config set 报错含 anthropic.model" grep -q "anthropic.model" c3.out

    "$REFLECT_BIN" config unset anthropic.model > c4.out 2>&1
    tcase "config unset 成功" test $? -eq 0
    "$REFLECT_BIN" config ls > c5.out 2>&1
    tcase "config unset 后回 <unset>" grep -q "anthropic.model  *=" c5.out && \
        grep "anthropic.model" c5.out | grep -q "<unset>"

    # 先落盘再 grep(管道 grep -q 提前退出会触发 reflect 的 EPIPE panic)
    "$REFLECT_BIN" config show > c6.out 2>&1
    tcase "config show 打印 ReflectConfig 头" grep -q "ReflectConfig @" c6.out
}

# ════════════════════════════════════════════════════════════════════════
# 组 3:mcp —— 双传输 add / 互斥 / ls / show / remove / test / registry
# ════════════════════════════════════════════════════════════════════════
cli_deep_mcp() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local mock_mcp="$ROOT/target/debug/mock_mcp_server"
    if [ ! -x "$mock_mcp" ]; then
        cargo build -p reflect-mcp --bin mock_mcp_server --quiet || true
    fi

    # stdio + http 两种传输
    "$REFLECT_BIN" mcp add fsx --command "$mock_mcp" > m1.out 2>&1
    tcase "mcp add stdio 成功" test $? -eq 0
    tcase "mcp add 回显 added" grep -q "added server 'fsx'" m1.out

    "$REFLECT_BIN" mcp add http1 --url "https://example.com/mcp" \
        --header "Authorization: Bearer x" > m2.out 2>&1
    tcase "mcp add http+header 成功" test $? -eq 0

    # --command 与 --url 互斥
    "$REFLECT_BIN" mcp add both1 --command x --url "https://e.com" > m3.out 2>&1
    tcase "mcp add 双传输互斥报错" test $? -eq 2
    tcase "mcp add 互斥提示" grep -q "cannot be used with" m3.out

    "$REFLECT_BIN" mcp ls > m4.out 2>&1
    tcase "mcp ls 含 stdio server" grep -q "fsx" m4.out
    tcase "mcp ls 含 http server" grep -q "http1" m4.out
    tcase "mcp ls 标注 stdio 传输" grep -q "stdio" m4.out

    "$REFLECT_BIN" mcp show fsx > m5.out 2>&1
    tcase "mcp show 含 stdio type" grep -q 'type = "stdio"' m5.out
    tcase "mcp show 含 command" grep -q "command" m5.out
    "$REFLECT_BIN" mcp show http1 > m6.out 2>&1
    tcase "mcp show http 含 url" grep -q "example.com/mcp" m6.out
    "$REFLECT_BIN" mcp show missing > m7.out 2>&1
    tcase "mcp show 不存在报错" test $? -ne 0
    tcase "mcp show 报错含 not in config" grep -q "not in config" m7.out

    # test:真实 mock server 成功(列出工具),坏 binary 失败
    if [ -x "$mock_mcp" ]; then
        "$REFLECT_BIN" mcp test fsx > m8.out 2>&1
        tcase "mcp test 真实 server 退出 0" test $? -eq 0
        tcase "mcp test 列出 echo 工具" grep -q "mcp__fsx__echo" m8.out
        tcase "mcp test 列出 slow 工具" grep -q "mcp__fsx__slow" m8.out
    fi
    "$REFLECT_BIN" mcp add bad --command /nonexistent/reflect-mcp-bad > /dev/null 2>&1
    "$REFLECT_BIN" mcp test bad > m9.out 2>&1
    tcase "mcp test 坏 binary 报错" test $? -ne 0
    tcase "mcp test 报错含 failed to start" grep -q "failed to start" m9.out

    "$REFLECT_BIN" mcp remove fsx > m10.out 2>&1
    tcase "mcp remove 成功" test $? -eq 0
    tcase "mcp remove 回显 removed" grep -q "removed server 'fsx'" m10.out
    "$REFLECT_BIN" mcp show fsx > /dev/null 2>&1
    tcase "mcp remove 后 show 报错" test $? -ne 0

    # registry:catalog 列表 / 有效 id / 未知 id
    "$REFLECT_BIN" mcp registry ls > m11.out 2>&1
    tcase "mcp registry ls 退出 0" test $? -eq 0
    tcase "mcp registry ls 含 filesystem" grep -q "filesystem" m11.out
    tcase "mcp registry ls 含 github" grep -q "github" m11.out
    tcase "mcp registry ls 含 postgres" grep -q "postgres" m11.out
    "$REFLECT_BIN" mcp registry show fetch > m12.out 2>&1
    tcase "mcp registry show 有效 id 退出 0" test $? -eq 0
    "$REFLECT_BIN" mcp registry show google-search > m13.out 2>&1
    tcase "mcp registry show 未知 id 报错" test $? -ne 0
    tcase "mcp registry 报错含 unknown" grep -q "unknown registry id" m13.out
}

# ════════════════════════════════════════════════════════════════════════
# 组 4:session —— 双 ID 前缀解析回归 + 生命周期
# ════════════════════════════════════════════════════════════════════════
cli_deep_session() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local base="$ISO_HOME/.reflect/sessions"
    # 真实 session(mock exec 产生一次)
    REFLECT_MODEL=mock "$REFLECT_BIN" exec "hello" > s0.jsonl 2>s0.err
    tcase "session 前置 exec 成功" test $? -eq 0
    local ids thread internal
    ids=$(real_session_ids)
    thread="${ids% *}"; internal="${ids#* }"

    # ── 双 ID 前缀解析(产品修复回归)──────────────────────────────
    "$REFLECT_BIN" session show "${thread:0:8}" > s1.out 2>&1
    tcase "session show 文件名(thread)前缀 8 位" test $? -eq 0
    tcase "session show 输出 Path" grep -q "Path:" s1.out
    tcase "session show 定位到正确文件" grep -q "${thread}.jsonl" s1.out

    "$REFLECT_BIN" session show "${internal:0:8}" > s2.out 2>&1
    tcase "session show 内部 id 前缀 8 位" test $? -eq 0
    tcase "session show 内部前缀输出 Path" grep -q "Path:" s2.out

    "$REFLECT_BIN" session show "${internal:0:12}" > s3.out 2>&1
    tcase "session show 内部 id 前缀 12 位" test $? -eq 0

    "$REFLECT_BIN" session show "$internal" > s4.out 2>&1
    tcase "session show 内部 id 完整" test $? -eq 0
    "$REFLECT_BIN" session show "$thread" > s5.out 2>&1
    tcase "session show 文件名 id 完整" test $? -eq 0
    tcase "session show 无匹配报错" bash -c "'$REFLECT_BIN' session show zz 2>&1 | grep -q 'no session matches prefix'"

    # ── 受控 fixture:内部 id 前缀歧义(两个 session 共享 8 位前缀)──
    write_fake_session "$base" \
        "11111111-0000-4000-8000-000000000001" \
        "11111111-0000-4000-8000-000000000001"
    write_fake_session "$base" \
        "11111111-0000-4000-8000-000000000002" \
        "11111111-0000-4000-8000-000000000002"
    "$REFLECT_BIN" session show 1111 > s6.out 2>&1
    tcase "session show 歧义前缀报错" test $? -ne 0
    tcase "session show 歧义提示 matches 2" grep -q "matches 2 sessions" s6.out
    "$REFLECT_BIN" session show "11111111-0000-4000-8000-000000000001" > s7.out 2>&1
    tcase "session show 完整内部 id 唯一定位" test $? -eq 0

    # ── 受控 fixture:文件名 id ≠ 内部 id(双 ID 兜底)────────────
    write_fake_session "$base" \
        "22222222-0000-4000-8000-000000000001" \
        "44444444-0000-4000-8000-000000000001"
    "$REFLECT_BIN" session show 44444444 > s8.out 2>&1
    tcase "session show 内部 id 前缀(文件名不同)" test $? -eq 0
    "$REFLECT_BIN" session show 22222222 > s9.out 2>&1
    tcase "session show 文件名前缀(内部 id 不同)" test $? -eq 0
    tcase "session show 双 ID 定位同一文件" grep -q "22222222-0000-4000-8000-000000000001.jsonl" s9.out

    # ── 生命周期:ls 过滤 / rename / export / fork / rm ──────────
    "$REFLECT_BIN" session ls > s10.out 2>&1
    tcase "session ls 含真实 session" grep -q "$internal" s10.out
    tcase "session ls 含 fixture session" grep -q "11111111-0000-4000-8000-000000000001" s10.out

    "$REFLECT_BIN" session ls --model mock > s11.out 2>&1
    tcase "session ls --model 过滤命中" grep -q "$internal" s11.out
    "$REFLECT_BIN" session ls --model no-such-model > s12.out 2>&1
    tcase "session ls --model 无匹配提示" grep -q "(no sessions found" s12.out
    "$REFLECT_BIN" session ls --limit 2 > s13.out 2>&1
    tcase "session ls --limit 2 行数受限" test "$(grep -c "  [0-9]" s13.out || true)" -le 3

    "$REFLECT_BIN" session rename "${internal:0:8}" "mytest-name" > s14.out 2>&1
    tcase "session rename 成功" test $? -eq 0
    tcase "session rename 回显" grep -q "Renamed session" s14.out
    "$REFLECT_BIN" session show "${internal:0:8}" > s15.out 2>&1
    tcase "session show 显示自定义 Name" grep -q "Name:    mytest-name" s15.out

    "$REFLECT_BIN" session export "$internal" > s16.out 2>&1
    tcase "session export stdout 成功" test $? -eq 0
    tcase "session export 输出 markdown 头" grep -q "# Reflect Session" s16.out
    "$REFLECT_BIN" session export "$internal" --out s17.md > /dev/null 2>&1
    tcase "session export --out 写文件" test -s s17.md

    "$REFLECT_BIN" session fork "$internal" --branch b1 > s18.out 2>&1
    tcase "session fork 成功" test $? -eq 0
    tcase "session fork 回显 Forked" grep -q "Forked session" s18.out

    # rm:用完整内部 id(前缀 11111111 命中两个 fixture,歧义会报错)。
    # 拒绝(读 stdin n)→ 文件保留;--yes → 删除
    local victim="$base/2026/08/01/11111111-0000-4000-8000-000000000001.jsonl"
    local vid="11111111-0000-4000-8000-000000000001"
    echo n | "$REFLECT_BIN" session rm "$vid" > s19.out 2>&1
    tcase "session rm 拒绝后文件保留" test -f "$victim"
    tcase "session rm 拒绝回显 aborted" grep -q "aborted" s19.out
    "$REFLECT_BIN" session rm "$vid" --yes > s20.out 2>&1
    tcase "session rm --yes 成功" test $? -eq 0
    tcase "session rm 后文件消失" test ! -f "$victim"
    "$REFLECT_BIN" session show "$vid" > /dev/null 2>&1
    tcase "session rm 后 show 报错" test $? -ne 0
}

# ════════════════════════════════════════════════════════════════════════
# 组 5:traces —— LLM 调用记录
# ════════════════════════════════════════════════════════════════════════
cli_deep_traces() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    REFLECT_MODEL=mock "$REFLECT_BIN" exec "trace-me" > t0.jsonl 2>t0.err
    tcase "traces 前置 exec 成功" test $? -eq 0

    # trace 文件布局:~/.reflect/traces/model-io/model-io-sess_<uuid>.jsonl
    local tfile tprefix
    tfile=$(find "$ISO_HOME/.reflect/traces/model-io" -name '*.jsonl' 2>/dev/null | head -1)
    tcase "traces 落盘 model-io" test -n "$tfile"
    if [ -n "$tfile" ]; then
        tprefix=$(basename "$tfile" .jsonl | sed 's/^model-io-sess_//' | cut -c1-8)
        "$REFLECT_BIN" traces show "$tprefix" > t1.out 2>&1
        tcase "traces show 前缀成功" test $? -eq 0
        tcase "traces show 含 Calls 段" grep -q "Calls:" t1.out
        tcase "traces show 含 model 行" grep -q "model:" t1.out
        tcase "traces show 含 usage 行" grep -q "usage:" t1.out
        "$REFLECT_BIN" traces show "$tprefix" > t2.out 2>&1
        tcase "traces show 含 request 段" grep -q "request:" t2.out
    fi

    "$REFLECT_BIN" traces ls > t3.out 2>&1
    tcase "traces ls 退出 0" test $? -eq 0
    tcase "traces ls 含 mock 模型名" grep -q "mock" t3.out
    "$REFLECT_BIN" traces ls -n 5 > t4.out 2>&1
    tcase "traces ls -n 5 退出 0" test $? -eq 0

    "$REFLECT_BIN" traces show deadbeef > t5.out 2>&1
    tcase "traces show 不存在报错" test $? -ne 0
    tcase "traces show 报错含 no trace session" grep -q "no trace session matches" t5.out
}

# ════════════════════════════════════════════════════════════════════════
# 组 6:task —— 结构化任务 + team 全链路
# ════════════════════════════════════════════════════════════════════════
cli_deep_task() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # create → 解析任务 id
    "$REFLECT_BIN" task create --subject "S1" --list L1 --description "d1" --active-form "Doing" > k1.out 2>&1
    tcase "task create 成功" test $? -eq 0
    tcase "task create 回显 Task #1" grep -q "Task #1 created in list 'L1'" k1.out
    local tid
    tid=$(sed -n "s/Task #\([0-9]*\) created.*/\1/p" k1.out)

    "$REFLECT_BIN" task ls > k2.out 2>&1
    tcase "task ls 含任务标题" grep -q "S1" k2.out
    tcase "task ls 状态 pending" grep -q "pending" k2.out

    # task show 必须带 --list(与 update/stop 同约定)
    "$REFLECT_BIN" task show "$tid" --list L1 > k3.out 2>&1
    tcase "task show 成功" test $? -eq 0
    tcase "task show 含 subject" grep -q "S1" k3.out
    tcase "task show 含 status pending" grep -q "pending" k3.out
    "$REFLECT_BIN" task show "$tid" > k3b.out 2>&1
    tcase "task show 缺 --list 报错" test $? -eq 2

    "$REFLECT_BIN" task update "$tid" --list L1 --status completed > k4.out 2>&1
    tcase "task update --status completed" test $? -eq 0
    "$REFLECT_BIN" task ls > k5.out 2>&1
    tcase "task update 后 ls 状态 completed" grep -q "completed" k5.out
    "$REFLECT_BIN" task update "$tid" --list L1 --subject "S1-renamed" > k6.out 2>&1
    tcase "task update --subject" test $? -eq 0
    "$REFLECT_BIN" task ls > k7.out 2>&1
    tcase "task update subject 生效" grep -q "S1-renamed" k7.out

    # 非数字 id:clap u32 解析失败(rc=2)
    "$REFLECT_BIN" task update abc --list L1 --status done > k8.out 2>&1
    tcase "task update 非数字 id 报错" test $? -eq 2
    "$REFLECT_BIN" task stop abc --list L1 > k9.out 2>&1
    tcase "task stop 非数字 id 报错" test $? -eq 2

    # team 全链路:create → ls → show → sync → delete
    "$REFLECT_BIN" task team create myteam --description "d" > k10.out 2>&1
    tcase "task team create 成功" test $? -eq 0
    "$REFLECT_BIN" task team ls > k11.out 2>&1
    tcase "task team ls 含 myteam" grep -q "myteam" k11.out
    "$REFLECT_BIN" task team show myteam > k12.out 2>&1
    tcase "task team show 成功" test $? -eq 0
    "$REFLECT_BIN" task team sync --list-specs > k13.out 2>&1
    tcase "task team sync --list-specs 退出 0" test $? -eq 0
    # 交互确认:显式喂 n(避免读真实 stdin 阻塞);--yes 跳过
    echo n | "$REFLECT_BIN" task team delete myteam > k14a.out 2>&1
    tcase "task team delete 拒绝后保留" \
        bash -c "'$REFLECT_BIN' task team ls | grep -q myteam"
    tcase "task team delete 拒绝回显 aborted" grep -q "aborted" k14a.out
    "$REFLECT_BIN" task team delete myteam --yes > k14.out 2>&1
    tcase "task team delete --yes 成功" test $? -eq 0
    "$REFLECT_BIN" task team ls > k15.out 2>&1
    tcase "task team delete 后 ls 不含" bash -c "! grep -q myteam k15.out"

    # stop 物理删除 + purge 级联清理
    "$REFLECT_BIN" task stop "$tid" --list L1 > k16.out 2>&1
    tcase "task stop 成功" test $? -eq 0
    "$REFLECT_BIN" task ls > k17.out 2>&1
    tcase "task stop 后 ls 不含任务" bash -c "! grep -q 'S1-renamed' k17.out"
    "$REFLECT_BIN" task purge --list L1 --yes > k18.out 2>&1
    tcase "task purge --list --yes 退出 0" test $? -eq 0
}

# ════════════════════════════════════════════════════════════════════════
# 组 7:plugin —— marketplace + 安装全链路
# ════════════════════════════════════════════════════════════════════════
cli_deep_plugin() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # marketplace fixture:directory 源 + 一个 hello 插件
    local mkt="$ISO_CWD/mkt"
    mkdir -p "$mkt/.claude-plugin" "$mkt/plugins/hello"
    cat > "$mkt/plugins/hello/plugin.toml" <<'EOF'
name = "hello"
version = "0.1.0"
description = "deep test plugin"
EOF
    cat > "$mkt/.claude-plugin/marketplace.json" <<EOF
{
  "name": "deep-mk",
  "owner": { "name": "Test" },
  "plugins": [
    { "name": "hello", "version": "0.1.0", "source": { "source": "directory", "path": "./plugins/hello" } }
  ]
}
EOF

    "$REFLECT_BIN" plugin marketplace add deep-mk --from directory --path "$mkt" > p1.out 2>&1
    tcase "plugin marketplace add 成功" test $? -eq 0
    tcase "plugin marketplace add 回显" grep -q "added marketplace 'deep-mk'" p1.out

    "$REFLECT_BIN" plugin marketplace ls > p2.out 2>&1
    tcase "plugin marketplace ls 含 deep-mk" grep -q "deep-mk" p2.out

    # 缺 manifest 的目录报错
    local badmkt="$ISO_CWD/badmkt"; mkdir -p "$badmkt"
    "$REFLECT_BIN" plugin marketplace add bad-mk --from directory --path "$badmkt" > p3.out 2>&1
    tcase "plugin marketplace add 缺 manifest 报错" test $? -ne 0
    tcase "plugin marketplace 报错含 manifest" grep -q "manifest" p3.out

    # 从 marketplace 安装 + 元数据查询
    "$REFLECT_BIN" plugin install "hello@deep-mk" > p4.out 2>&1
    tcase "plugin install from marketplace 成功" test $? -eq 0
    tcase "plugin install 回显 installed" grep -q "hello@deep-mk" p4.out

    "$REFLECT_BIN" plugin list > p5.out 2>&1
    tcase "plugin list 含 hello@deep-mk" grep -q "hello@deep-mk" p5.out

    "$REFLECT_BIN" plugin info "hello@deep-mk" > p6.out 2>&1
    tcase "plugin info 成功" test $? -eq 0
    tcase "plugin info 含 scope user" grep -q "user" p6.out
    tcase "plugin info 含 version" grep -q "0.1.0" p6.out

    "$REFLECT_BIN" plugin show "hello@deep-mk" > p7.out 2>&1
    tcase "plugin show 输出 manifest" grep -q 'name = "hello"' p7.out

    # 启用/禁用:config.toml#plugins.enabled_plugins 双向
    "$REFLECT_BIN" plugin enable "hello@deep-mk" > p8.out 2>&1
    tcase "plugin enable 成功" test $? -eq 0
    tcase "plugin enable 写入 config" \
        grep -q 'enabled_plugins = \["hello@deep-mk"\]' "$ISO_HOME/.reflect/config.toml"
    "$REFLECT_BIN" plugin disable "hello@deep-mk" > p9.out 2>&1
    tcase "plugin disable 成功" test $? -eq 0
    tcase "plugin disable 移除 config" \
        bash -c "! grep -q 'hello@deep-mk' '$ISO_HOME/.reflect/config.toml'"

    # 卸载 + 卸载后 info 报错
    "$REFLECT_BIN" plugin uninstall "hello@deep-mk" --yes > p10.out 2>&1
    tcase "plugin uninstall --yes 成功" test $? -eq 0
    tcase "plugin uninstall 回显" grep -q "uninstalled" p10.out
    "$REFLECT_BIN" plugin info "hello@deep-mk" > p11.out 2>&1
    tcase "plugin info 已卸载报错" test $? -ne 0
    tcase "plugin info 报错含 not installed" grep -q "not installed" p11.out

    # 本地目录直接安装(inline scope)+ marketplace remove
    "$REFLECT_BIN" plugin install "$mkt/plugins/hello" > p12.out 2>&1
    tcase "plugin install 本地目录成功" test $? -eq 0
    tcase "plugin install inline 回显" grep -q "hello@inline" p12.out
    "$REFLECT_BIN" plugin marketplace refresh > p13.out 2>&1
    tcase "plugin marketplace refresh 退出 0" test $? -eq 0
    "$REFLECT_BIN" plugin marketplace remove deep-mk > p14.out 2>&1
    tcase "plugin marketplace remove 成功" test $? -eq 0
    tcase "plugin marketplace remove 回显" grep -q "removed marketplace 'deep-mk'" p14.out
    "$REFLECT_BIN" plugin marketplace ls > p15.out 2>&1
    tcase "plugin marketplace remove 后 ls 不含" bash -c "! grep -q deep-mk p15.out"
}

# ════════════════════════════════════════════════════════════════════════
# 组 8:workspace / update / doctor / lsp / security
# ════════════════════════════════════════════════════════════════════════
cli_deep_misc() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # ── workspace ────────────────────────────────────────────────────
    tcase "workspace ls 打印提示" bash -c "'$REFLECT_BIN' workspace ls | grep -q workspace"

    # 本地 git 仓库克隆(离线)
    git init -q clone_src
    (cd clone_src && git config user.email t@t && git config user.name t \
        && echo hello > f.txt && git add f.txt && git commit -qm init)
    "$REFLECT_BIN" workspace clone "$ISO_CWD/clone_src" --dest cloned > w1.out 2>&1
    tcase "workspace clone 本地仓库成功" test -f cloned/f.txt
    # --depth 浅克隆:git 对本地路径忽略 --depth(须 file:// URL 才产生 shallow)
    "$REFLECT_BIN" workspace clone "file://$ISO_CWD/clone_src" --dest cloned_shallow --depth 1 > w2.out 2>&1
    tcase "workspace clone file:// --depth 1 成功" test -f cloned_shallow/f.txt
    tcase "workspace clone --depth 1 产生 shallow 标记" test -f cloned_shallow/.git/shallow
    git -C cloned_shallow rev-parse --is-shallow-repository > w2b.out 2>&1
    tcase "shallow 仓库自报 true" grep -q "true" w2b.out

    # 坏 URL:DNS 解析失败,git clone 报错
    "$REFLECT_BIN" workspace clone "https://invalid.invalid/nope" --dest wbad > w3.out 2>&1
    tcase "workspace clone 坏 URL 报错" test $? -ne 0
    tcase "workspace clone 报错含 git clone" grep -q "git clone" w3.out

    # ── update ───────────────────────────────────────────────────────
    "$REFLECT_BIN" update > u1.out 2>&1
    tcase "update 打印版本" grep -q "reflect v" u1.out
    tcase "update 含 cargo install 提示" grep -q "cargo install" u1.out
    "$REFLECT_BIN" update --check-only > u2.out 2>&1
    tcase "update --check-only 退出 0" test $? -eq 0
    tcase "update --check-only 标注预留" grep -q "check-only" u2.out

    # ── doctor ───────────────────────────────────────────────────────
    "$REFLECT_BIN" doctor > d1.out 2>&1
    tcase "doctor 退出 0" test $? -eq 0
    tcase "doctor 含 config 检查段" grep -q "config" d1.out
    tcase "doctor 含 rollout 检查段" grep -q "rollout" d1.out
    tcase "doctor 无网络探针时 skip" grep -q "network probe skipped" d1.out
    # mock provider 下探测 DNS(mock:443 必然解析失败,但 best-effort 不致命)
    REFLECT_MODEL=mock "$REFLECT_BIN" doctor --check-network > d2.out 2>&1
    tcase "doctor --check-network 退出 0(best-effort)" test $? -eq 0
    tcase "doctor --check-network 含 network 段" grep -q "\[network\]" d2.out
    tcase "doctor --check-network 标注 checking provider" grep -q "checking provider" d2.out

    # ── lsp ──────────────────────────────────────────────────────────
    "$REFLECT_BIN" lsp list > l1.out 2>&1
    tcase "lsp list 空配置提示" grep -q "no \[lsp_servers" l1.out
    "$REFLECT_BIN" lsp status rust > l2.out 2>&1
    tcase "lsp status 未配置报错" test $? -ne 0
    tcase "lsp status 报错含 not configured" grep -q "not configured" l2.out

    # 写入 LSP 配置后:list / status 均应可见
    cat >> "$ISO_HOME/.reflect/config.toml" <<'EOF'

[lsp_servers.rust]
command = "rust-analyzer"
file_patterns = [{ glob = "**/*.rs", language_id = "rust" }]
EOF
    "$REFLECT_BIN" lsp list > l3.out 2>&1
    tcase "lsp list 配置后含 rust" grep -q "rust-analyzer" l3.out
    "$REFLECT_BIN" lsp status rust > l4.out 2>&1
    tcase "lsp status 配置后成功" test $? -eq 0
    tcase "lsp status 含 command" grep -q "rust-analyzer" l4.out
    tcase "lsp status 含 patterns" grep -q "\*\*/\*.rs" l4.out

    # ── security audit(wrapper 尽力而为,退出 0)──────────────────
    "$REFLECT_BIN" security audit > sec.out 2>&1
    tcase "security audit 退出 0" test $? -eq 0
    tcase "security audit 打印报告头" grep -q "Reflect security audit" sec.out
}

# ════════════════════════════════════════════════════════════════════════
# 组 9:pipeline / discussion —— 错误路径 + mock 真多 agent
# ════════════════════════════════════════════════════════════════════════
cli_deep_pipeline_discussion() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # ── pipeline:错误路径(happy path 受预设模板限制,见覆盖矩阵)──
    "$REFLECT_BIN" pipeline team ghost --topic t > pp1.out 2>&1
    tcase "pipeline team 不存在报错" test $? -ne 0
    tcase "pipeline team 报错含 team not found" grep -q "team not found: ghost" pp1.out

    # 自定义 label 无 runner(只认 plan/prd/exec/verify 预设)
    "$REFLECT_BIN" task team create only > /dev/null 2>&1
    cat > pipe.toml <<'EOF'
[pipeline]
name = "deep"
failure_policy = "abort"

[nodes.only]
team = "only"
template = "Do: {{topic}}"
depends_on = []
EOF
    "$REFLECT_BIN" pipeline run -c pipe.toml --topic "t1" > pp2.out 2>&1
    tcase "pipeline run 自定义 label 报错" test $? -ne 0
    tcase "pipeline run 报错含 no runner" grep -q "no runner for node 'only'" pp2.out

    # ── discussion:mock 驱动 sequential 真多 agent ──────────────────
    local s="$ISO_CWD/scripts"; mkdir -p "$s"
    make_mock_script "$s/disc.jsonl" \
        '{"type":"tool_call","name":"send_message","args":{"content":"advocate says async","kind":"utterance"}}' \
        '{"type":"tool_call","name":"send_message","args":{"content":"skeptic says sync","kind":"utterance"}}' \
        '{"type":"text","text":"a"}' \
        '{"type":"text","text":"b"}'
    cat > disc.toml <<'EOF'
[discussion]
mode = "sequential"
topic = "deep-topic"
max_rounds = 1

[[agents]]
role = "advocate"
system_prompt = "advocate"
allowed_tools = ["send_message", "read_messages", "finish_discussion"]

[[agents]]
role = "skeptic"
system_prompt = "skeptic"
allowed_tools = ["send_message", "read_messages", "finish_discussion"]
EOF
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/disc.jsonl" \
        "$REFLECT_BIN" discussion run -c disc.toml > dr1.out 2>dr1.err
    tcase "discussion run(mock)退出 0" test $? -eq 0
    tcase "discussion run 输出 Transcript 段" grep -q "=== Discussion Transcript ===" dr1.out
    tcase "discussion run 输出 Result 段" grep -q "=== Result ===" dr1.out
    tcase "discussion run Result 含 rounds_completed" grep -q "rounds_completed" dr1.out
    tcase "discussion run Result 含 transcript_len=2" grep -q '"transcript_len": 2' dr1.out
    tcase "discussion run transcript 含 advocate 发言" grep -q "advocate says async" dr1.out
    tcase "discussion run transcript 含 skeptic 发言" grep -q "skeptic says sync" dr1.out
    tcase "discussion run 输出 collab_message 事件" grep -q '"collab_message"' dr1.out

    # --output 文件模式:实测 -o 写的是完整报告(transcript 段 + Result JSON),
    # 不是纯 JSON 文件
    REFLECT_MODEL=mock REFLECT_MOCK_SCRIPT="$s/disc.jsonl" \
        "$REFLECT_BIN" discussion run -c disc.toml -o dr2.json > dr2.out 2>dr2.err
    tcase "discussion run --output 写文件" test -s dr2.json
    tcase "discussion --output 文件含 Result JSON" grep -q '"rounds_completed"' dr2.json
    tcase "discussion --output 文件含 transcript 段" grep -q "Discussion Transcript" dr2.json

    # discussion ls:mock 路径不写 DiscussionTranscript rollout(实测行为),
    # 断言退出 0 与空列表提示;--discussion-id 过滤同样退出 0。
    "$REFLECT_BIN" discussion ls > dr3.out 2>&1
    tcase "discussion ls 退出 0" test $? -eq 0
    tcase "discussion ls 空列表提示" grep -q "(no sessions found" dr3.out
    "$REFLECT_BIN" discussion ls \
        --discussion-id 00000000-0000-4000-8000-000000000000 > dr4.out 2>&1
    tcase "discussion ls --discussion-id 退出 0" test $? -eq 0
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
cli_deep_login
cli_deep_config
cli_deep_mcp
cli_deep_session
cli_deep_traces
cli_deep_task
cli_deep_plugin
cli_deep_misc
cli_deep_pipeline_discussion

tfinish "cli_deep.sh" || exit 1
exit 0
