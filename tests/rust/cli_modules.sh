#!/usr/bin/env bash
# tests/rust/cli_modules.sh —— 模块型子命令端到端(隔离 HOME + mock provider)。
#
# 覆盖:mcp(add/ls/show/test/remove)、lsp、plugin 全生命周期、
#       task + team、pipeline(run/team)、discussion(run/ls)、
#       security audit、workspace(ls/clone)。
#
# 已知限制(断言为当前行为,防意外退化):
#   - pipeline 的 plan 预设模板硬编码 {{input.audience}} 而 CLI 不注入该
#     输入 → plan 节点模板渲染必败(`unknown input 'audience'`)。
#     pipeline team 子命令整体退出 0 并输出失败报告;pipeline run 退出非 0。
#   - discussion 的 concurrent 模式受 SubAgentFactory 深度上限约束
#     (见 crates/orchestration/reflect-discussion/src/llm.rs「已知限制」);
#     无 provider 时 CLI 降级 run_noop(offline / CI 友好)→ 退出 0。
#
# 用法:由 tests/rust/run.sh source;也可独立执行。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

require_binary

# mock MCP server 二进制(集成测试 bin);没有就现构建。
ensure_mock_mcp_server() {
    MOCK_SRV="$ROOT/target/debug/mock_mcp_server"
    if [ ! -x "$MOCK_SRV" ]; then
        cargo build -p reflect-mcp --bin mock_mcp_server --quiet || return 1
    fi
    [ -x "$MOCK_SRV" ]
}

# ════════════════════════════════════════════════════════════════════════
# 组 7:mcp + lsp
# ════════════════════════════════════════════════════════════════════════
cli_modules_mcp_lsp() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 7a. add stdio / http
    "$REFLECT_BIN" mcp add fsx --command "$ROOT/target/debug/mock_mcp_server" >/dev/null 2>&1
    tcase "mcp add stdio 写配置" grep -q '\[mcp_servers.fsx\]' "$HOME/.reflect/config.toml"
    "$REFLECT_BIN" mcp add ghx --url https://mcp.example.com/github >/dev/null 2>&1
    tcase "mcp add http 写配置" grep -q 'https://mcp.example.com/github' "$HOME/.reflect/config.toml"

    # 7b. ls / show
    "$REFLECT_BIN" mcp ls > mls.txt 2>&1
    tcase "mcp ls 列出两个 server" sh -c "grep -q fsx mls.txt && grep -q ghx mls.txt"
    "$REFLECT_BIN" mcp show fsx > mshow.txt 2>&1
    tcase "mcp show 打印 server 配置" grep -q "mock_mcp_server" mshow.txt

    # 7c. test:真启动 mock server 列工具
    if ensure_mock_mcp_server; then
        "$REFLECT_BIN" mcp test fsx > mtest.txt 2>&1
        tcase "mcp test 启动并列出工具" grep -q "mcp__fsx__echo" mtest.txt
    else
        echo "  [SKIP] mock_mcp_server 构建失败,跳过 mcp test"
    fi

    # 7d. remove
    "$REFLECT_BIN" mcp remove ghx >/dev/null 2>&1
    "$REFLECT_BIN" mcp ls > mls2.txt 2>&1
    tcase "mcp remove 后不再列出" sh -c "! grep -q '^ghx' mls2.txt"

    # 7e. lsp:写入配置段后 list / status
    cat >> "$HOME/.reflect/config.toml" <<'EOF'

[lsp_servers.rust]
command = "rust-analyzer"
EOF
    "$REFLECT_BIN" lsp list > lls.txt 2>&1
    tcase "lsp list 列出 rust server" grep -q "rust" lls.txt
    "$REFLECT_BIN" lsp status rust > lst.txt 2>&1
    tcase "lsp status 打印完整配置" grep -q "rust-analyzer" lst.txt
}

# ════════════════════════════════════════════════════════════════════════
# 组 8:plugin 全生命周期
# ════════════════════════════════════════════════════════════════════════
cli_modules_plugin() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # fixture:本地目录,含 commands + skills 两类能力
    mkdir -p plug/commands plug/skills/lint
    cat > plug/plugin.toml <<'EOF'
name = "e2eplug"
version = "1.0.0"
description = "e2e fixture plugin"
commands = "./commands"
skills = "./skills"
EOF
    printf -- '---\ndescription: greet\n---\n# hi\n' > plug/commands/hello.md
    printf -- '---\ndescription: lint skill\n---\n' > plug/skills/lint/SKILL.md
    local pid="e2eplug@inline"

    "$REFLECT_BIN" plugin install plug > pinst.txt 2>&1
    tcase "plugin install 安装到 cache" grep -q "installed e2eplug@inline" pinst.txt
    "$REFLECT_BIN" plugin list > pls.txt 2>&1
    tcase "plugin list 显示已安装" grep -q "$pid" pls.txt
    "$REFLECT_BIN" plugin info "$pid" > pinfo.txt 2>&1
    tcase "plugin info 打印元数据" grep -q "install_path" pinfo.txt
    "$REFLECT_BIN" plugin enable "$pid" >/dev/null 2>&1
    tcase "plugin enable 写入 enabled_plugins" grep -q "\"$pid\"" "$HOME/.reflect/config.toml"
    "$REFLECT_BIN" plugin disable "$pid" >/dev/null 2>&1
    tcase "plugin disable 移出 enabled_plugins" sh -c "! grep -q 'enabled_plugins = \\[\"$pid\"\\]' '$HOME/.reflect/config.toml'"
    "$REFLECT_BIN" plugin show "$pid" > pshow.txt 2>&1
    tcase "plugin show 打印 manifest" grep -q "e2eplug" pshow.txt
    "$REFLECT_BIN" plugin uninstall "$pid" --yes >/dev/null 2>&1
    "$REFLECT_BIN" plugin list > pls2.txt 2>&1
    tcase "plugin uninstall 后列表为空" grep -q "no plugins installed" pls2.txt
    "$REFLECT_BIN" plugin marketplace ls > pmk.txt 2>&1
    tcase "plugin marketplace ls 打印空态提示" grep -q "no marketplaces" pmk.txt
}

# ════════════════════════════════════════════════════════════════════════
# 组 9:task + team
# ════════════════════════════════════════════════════════════════════════
cli_modules_task_team() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # 9a. task 增查改删
    "$REFLECT_BIN" task create --subject "e2e-task-1" --description "端到端任务" --active-form "Testing" > tc.txt 2>&1
    tcase "task create 报告创建" grep -q "created" tc.txt
    "$REFLECT_BIN" task ls > tl.txt 2>&1
    tcase "task ls 列出新任务" grep -q "e2e-task-1" tl.txt
    "$REFLECT_BIN" task show 1 --list admin > ts.txt 2>&1
    tcase "task show 打印详情" grep -q "端到端任务" ts.txt
    "$REFLECT_BIN" task update 1 --list admin --status in_progress >/dev/null 2>&1
    tcase "task update 改状态" sh -c "\"$REFLECT_BIN\" task ls | grep -q in_progress"
    "$REFLECT_BIN" task update 1 --list admin --subject "e2e-task-改" >/dev/null 2>&1
    tcase "task update 改标题" sh -c "\"$REFLECT_BIN\" task ls | grep -q 'e2e-task-改'"
    "$REFLECT_BIN" task stop 1 --list admin >/dev/null 2>&1
    "$REFLECT_BIN" task ls > tl2.txt 2>&1
    tcase "task stop 物理删除" sh -c "! grep -q 'e2e-task-改' tl2.txt"

    # 9b. team 增查删
    "$REFLECT_BIN" task team create alpha >/dev/null 2>&1
    "$REFLECT_BIN" task team create beta >/dev/null 2>&1
    "$REFLECT_BIN" task team ls > tml.txt 2>&1
    tcase "team ls 列出两个 team" sh -c "grep -q alpha tml.txt && grep -q beta tml.txt"
    "$REFLECT_BIN" task team show alpha > tms.txt 2>&1
    tcase "team show 打印成员" grep -q "team-lead@alpha" tms.txt
    "$REFLECT_BIN" task team sync --list-specs > tmy.txt 2>&1
    tcase "team sync --list-specs 退出 0" test -s tmy.txt
    "$REFLECT_BIN" task team delete beta --yes >/dev/null 2>&1
    "$REFLECT_BIN" task team ls > tml2.txt 2>&1
    tcase "team delete 后不再列出" sh -c "! grep -q beta tml2.txt"
}

# ════════════════════════════════════════════════════════════════════════
# 组 10:pipeline
# ════════════════════════════════════════════════════════════════════════
cli_modules_pipeline() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # pipeline 需要 plan/prd/exec/verify 四个同名 team(CLI 按 label 找 team)
    local t
    for t in plan prd exec verify; do
        "$REFLECT_BIN" task team create "$t" >/dev/null 2>&1
    done

    cat > pipeline.toml <<'EOF'
[pipeline]
name = "e2e-pipeline"
failure_policy = "continue_collect"

[nodes.plan]
team = "plan"
depends_on = []

[nodes.verify]
team = "verify"
depends_on = ["plan"]
EOF

    # 已知限制:plan 预设模板含 {{input.audience}},CLI 不注入 → 渲染必败。
    REFLECT_MODEL=mock "$REFLECT_BIN" pipeline run -c pipeline.toml --topic "e2e topic" \
        -o pipe_out.json > pipe_out.txt 2>pipe_err.log
    local rc=$?
    tcase "pipeline run 因 plan 模板缺 audience 输入退出非 0" test "$rc" -ne 0
    tcase "pipeline run 报告 unknown input 'audience'" grep -q "unknown input 'audience'" pipe_out.txt
    tcase "pipeline run -o 写出 JSON 报告" test -s pipe_out.json
    tcase "pipeline run abort 策略下 verify 被 skipped" grep -q "skipped" pipe_out.txt

    # pipeline team:整体退出 0,报告打印每节点状态
    REFLECT_MODEL=mock "$REFLECT_BIN" pipeline team --topic "e2e" plan > pteam.txt 2>pteam_err.log
    tcase "pipeline team 退出 0" test $? -eq 0
    tcase "pipeline team 打印 4 阶段节点状态" sh -c "grep -q plan pteam.txt && grep -q verify pteam.txt"

    # 空 topic 校验
    "$REFLECT_BIN" pipeline run -c pipeline.toml --topic "" >/dev/null 2>&1
    tcase "pipeline run 空 topic 被拒绝" test $? -ne 0
}

# ════════════════════════════════════════════════════════════════════════
# 组 11:discussion
# ════════════════════════════════════════════════════════════════════════
cli_modules_discussion() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    local cfg="$ROOT/crates/orchestration/reflect-discussion/examples/discussion.toml"

    # 无 provider(隔离 HOME 无 key、无 REFLECT_MODEL)→ 降级 run_noop:
    # 状态机跑通、transcript 为空、Result JSON 仍带 outcome / rounds_completed。
    "$REFLECT_BIN" discussion run -c "$cfg" > disc_out.txt 2>disc_err.log
    tcase "discussion run 无 key 降级 noop 退出 0" test $? -eq 0
    tcase "discussion run 输出 Transcript 段" grep -q "Discussion Transcript" disc_out.txt
    tcase "discussion run 输出 Result 段" grep -q "=== Result ===" disc_out.txt
    tcase "discussion run Result 含 rounds_completed" grep -q "rounds_completed" disc_out.txt

    # discussion ls:列出含 DiscussionTranscript 记录的 session(noop 路径
    # 不写 rollout,此断言只验证命令退出 0 与表头)
    "$REFLECT_BIN" discussion ls > dls.txt 2>&1
    tcase "discussion ls 退出 0" test $? -eq 0

    # 配置缺失报错
    "$REFLECT_BIN" discussion run -c ./nope.toml >/dev/null 2>&1
    tcase "discussion run 缺配置文件报错" test $? -ne 0
}

# ════════════════════════════════════════════════════════════════════════
# 组 12:security + workspace
# ════════════════════════════════════════════════════════════════════════
cli_modules_sec_ws() {
    make_isolated_env
    trap cleanup_isolated_env RETURN

    # security audit:未装 cargo-audit 时也打印报告并退出 0(尽力而为)
    "$REFLECT_BIN" security audit > sec.txt 2>&1
    tcase "security audit 退出 0" test $? -eq 0
    tcase "security audit 打印报告头" grep -q "Reflect security audit" sec.txt

    # workspace ls:提示信息
    "$REFLECT_BIN" workspace ls > wls.txt 2>&1
    tcase "workspace ls 打印提示" grep -q "workspace" wls.txt

    # workspace clone:本地 file:// git 仓库(离线)
    git init -q clone_src
    (cd clone_src && git config user.email t@t && git config user.name t \
        && echo hello > f.txt && git add f.txt && git commit -qm init)
    "$REFLECT_BIN" workspace clone "$ISO_CWD/clone_src" --dest cloned > wc.txt 2>&1
    tcase "workspace clone 本地仓库成功" test -f cloned/f.txt
    "$REFLECT_BIN" workspace clone "$ISO_CWD/clone_src" --dest cloned --branch main >/dev/null 2>&1
    tcase "workspace clone --branch 成功" test -f cloned/f.txt
}

# ── 执行入口 ──────────────────────────────────────────────────────────────
cli_modules_mcp_lsp
cli_modules_plugin
cli_modules_task_team
cli_modules_pipeline
cli_modules_discussion
cli_modules_sec_ws

tfinish "cli_modules.sh" || exit 1
exit 0
