"""能力覆盖层 · 缺陷修复回归。

直接驱动 `zhuye serve` / `zhuye` CLI 原始 wire,断言到**字段级**。覆盖:
- 迭代 / token 预算耗尽且零文本 → error 事件 `iteration_budget_exhausted` /
  `token_budget_exhausted`(强制收口产出了文本则不误报);
- `ZHUYE_HOME` 重定向 config.toml 读取路径(配置隔离);
- `[active] tools` / `[active] disallowed_tools` 主 agent 工具裁切;
- 提示词覆盖 `system.plan-mode = off` → core prompt 不再含 Plan Mode 段;
- mock 脚本配置错误显性化(全部行解析失败 / 路径不存在 → `INVALID_REQUEST`);
- mock 无脚本回退文案自带「mock 固定回复」可辨识标记。

`ZHUYE_MODEL` 的 provider 前缀剥离与 base_url 版本段拼接分别落在
zhuye-config / zhuye-llm 的 crate 单测 —— CLI 面不展示解析后模型、
serve 离线路径打不到真实网关,功能层无观测点。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_bugfixes.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import Sandbox, ServeSession, require_binary, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")


# ── 观测辅助 ─────────────────────────────────────────────────────────────


def _agent_text(events: list[dict[str, Any]]) -> str:
    """聚合一条 submission 的 agent_message_delta / agent_message 文本。"""
    parts: list[str] = []
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "agent_message_delta":
            parts.append(m.get("delta", ""))
        elif m.get("type") == "agent_message":
            parts.append(m.get("text", ""))
    return "".join(parts)


def _request_echo_payload(events: list[dict[str, Any]]) -> dict[str, Any]:
    """从 mock 的 request_echo 回复文本里解析观测快照。"""
    text = _agent_text(events)
    assert text, "request_echo 应回复一段 JSON 文本"
    return json.loads(text)


def _errors_of(events: list[dict[str, Any]], all_events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """收集某 submission 的 error 事件(Error 事件挂 submission id)。

    正常路径 error 已在 `events`(按 sub id 分桶)里;兼容 serve 把
    error 发成全局事件(id="")的历史形态,故同时扫 all_events。
    """
    ids = {ev.get("id") for ev in events}
    merged = {id(ev): ev for ev in events}
    out: list[dict[str, Any]] = []
    for ev in all_events:
        if ev.get("msg", {}).get("type") != "error":
            continue
        if ev.get("id") in ids or ev.get("id") == "":
            if id(ev) not in merged:
                out.append(ev)
    return [ev for ev in events if ev.get("msg", {}).get("type") == "error"] + out


# ── 预算耗尽显性化 ───────────────────────────────────────────────────────


def test_迭代预算耗尽零文本_发_iteration_budget_exhausted(
    serve_session, mock_script
) -> None:
    """预算 1、脚本两行连续工具调用 → 强制收口轮拿到的是
    工具调用(零文本)→ turn 不得再伪装成「正常空回复」,必须在
    turn_complete 之前发一条 code=iteration_budget_exhausted 的 error 事件。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"echo","args":{"text":"x"}}',
            '{"type":"tool_call","name":"echo","args":{"text":"y"}}',
        ]
    )
    with serve_session(
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_MAX_ITERATIONS": "1"}
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        errors = _errors_of(events, s.all_events())
        assert errors, "预算耗尽且零文本必须发 error 事件"
        err = errors[0]["msg"]
        assert err["code"] == "iteration_budget_exhausted", f"字段级断言: {err}"
        assert "迭代预算耗尽" in err["message"], err
        assert err["details"]["iterations"] >= 1, err
        # error 必须先于 turn_complete(上层先抛异常再收尾)。
        types = [e["msg"]["type"] for e in events]
        assert "turn_complete" in types, f"应有 turn_complete 收尾: {types}"
        assert "error" in types, f"error 事件应挂在该 submission 上: {types}"
        assert types.index("error") < types.index("turn_complete"), types
        # turn_complete.status = max_iterations(既有语义不变)。
        tc = next(e for e in events if e["msg"]["type"] == "turn_complete")
        assert tc["msg"]["status"] == "max_iterations"


def test_token预算耗尽零文本_发_token_budget_exhausted(
    serve_session, mock_script
) -> None:
    """token 预算 1、脚本一行工具调用(usage 4 token)→
    budget_exceeded 且零文本 → error 事件 token_budget_exhausted。"""
    require_binary()
    script = mock_script(['{"type":"tool_call","name":"echo","args":{"text":"x"}}'])
    with serve_session(
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_TOKEN_BUDGET": "1"}
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        errors = _errors_of(events, s.all_events())
        assert errors, "token 预算耗尽且零文本必须发 error 事件"
        assert errors[0]["msg"]["code"] == "token_budget_exhausted", errors[0]["msg"]
        tc = next(
            (e for e in events if e["msg"]["type"] == "turn_complete"), None
        )
        assert tc is not None and tc["msg"]["status"] == "token_budget_exceeded"


def test_预算耗尽但有最终答案_不误报_error(serve_session, mock_script) -> None:
    """反向守护:预算耗尽但强制收口轮产出了文本(合法完成)→
    **不得**发 error 事件(避免把正常收口当故障)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"echo","args":{"text":"x"}}',
            '{"type":"text","text":"final answer here"}',
        ]
    )
    with serve_session(
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_MAX_ITERATIONS": "1"}
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        errors = _errors_of(events, s.all_events())
        assert not errors, "收口成功(文本非空)不应发 error 事件"
        assert _agent_text(events).strip() != ""


# ── ZHUYE_HOME 配置隔离 ─────────────────────────────────────────────────


def test_ZHUYE_HOME_重定向_config_toml(sandbox: Sandbox, tmp_path) -> None:
    """`ZHUYE_HOME` 目录下的 config.toml 必须被 config show /
    serve 读到(与 session-notes 同一套语义),不再读 $HOME/.zhuye。"""
    require_binary()
    # ZHUYE_HOME 目录:自带一份带独特标记值的 config.toml。
    zh_home = tmp_path / "zh-home"
    zh_home.mkdir()
    (zh_home / "config.toml").write_text(
        "[hooks]\nenabled = []\n\n[compact]\ntrigger_tokens = 7777\n"
    )
    # 隔离 $HOME 的 config.toml 用不同值(证明读的是 ZHUYE_HOME 那份)。
    sandbox.write_config("\n[compact]\ntrigger_tokens = 1111\n")

    out = run_cli(
        sandbox, "config", "show", env={"ZHUYE_HOME": str(zh_home)}, timeout=60
    )
    assert out.returncode == 0, f"config show 失败: {out.stderr[:400]}"
    assert "7777" in out.stdout, (
        f"config show 应读 $ZHUYE_HOME/config.toml(含 trigger_tokens=7777);"
        f"实得: {out.stdout[:600]}"
    )
    assert "1111" not in out.stdout, "不得再读 $HOME/.zhuye/config.toml"


# ── [active] 工具裁切 ───────────────────────────────────────────────────


def _register_and_probe(serve_session, mock_script, tool: str = "get_weather") -> dict[str, Any]:
    """注册一个远程工具,用 request_echo 探测引擎实际发给模型的工具集。"""
    script = mock_script(['{"type":"request_echo"}'])
    s = serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)})
    # register_tools 无回执(T3 同款 fire-and-forget);serve 按 stdin
    # FIFO 处理,先于后续 user_input 生效。
    s.send(
        {
            "type": "register_tools",
            "tools": [
                {
                    "name": tool,
                    "description": "probe tool",
                    "parameters": {"type": "object", "properties": {}},
                }
            ],
        },
    )
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    return _request_echo_payload(events)


def test_active_tools_白名单裁掉内置工具(serve_session, mock_script, sandbox) -> None:
    """`[active] tools = ["get_weather"]` → 引擎发给模型的工具集
    只剩白名单内的工具(16 个内置工具不再随行)。"""
    require_binary()
    sandbox.write_config('\n[active]\ntools = ["get_weather"]\n')
    payload = _register_and_probe(serve_session, mock_script)
    tools = payload["tools"]
    assert tools == ["get_weather"], f"白名单应裁到只剩 get_weather: {tools}"
    assert "bash" not in tools and "write" not in tools, tools


def test_active_disallowed_tools_黑名单排除(serve_session, mock_script, sandbox) -> None:
    """`[active] disallowed_tools = ["bash"]` → bash 从模型可见
    工具集中消失,其余(如 read)保留。"""
    require_binary()
    sandbox.write_config('\n[active]\ndisallowed_tools = ["bash"]\n')
    payload = _register_and_probe(serve_session, mock_script)
    tools = payload["tools"]
    assert "bash" not in tools, f"黑名单工具不得出现: {tools}"
    assert "read" in tools, f"未列入黑名单的工具应保留: {tools}"
    assert "get_weather" in tools, f"客户端注册的远程工具应保留: {tools}"


def test_无配置时内置工具仍默认暴露(serve_session, mock_script) -> None:
    """向后兼容:不写 [active] 工具配置 → 行为与此前一致(内置
    工具全量暴露)。"""
    require_binary()
    payload = _register_and_probe(serve_session, mock_script)
    tools = payload["tools"]
    assert "bash" in tools and "read" in tools, f"默认应含内置工具: {tools}"
    assert "get_weather" in tools, f"远程工具应在场: {tools}"


# ── Plan Mode 段可关闭 ─────────────────────────────────────────────────


def test_plan_mode提示词可经覆盖关闭(serve_session, mock_script, sandbox) -> None:
    """`~/.zhuye/prompts/default.md` 把 `## system.plan-mode` 覆盖为
    `off` → 引擎发出的 system prompt 不再含 `## Plan Mode` 规则段;
    自定义 `## system.main` 全文生效。"""
    require_binary()
    prompts = sandbox.home / ".zhuye" / "prompts"
    prompts.mkdir(parents=True, exist_ok=True)
    (prompts / "default.md").write_text(
        "## system.main\n你是金融预测分析师(自定义 persona)。\n\n"
        "## system.plan-mode\noff\n\n"
        "## runtime.active-mode\noff\n"
    )
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        payload = _request_echo_payload(events)
    # 只断言 core 块(system[0]):ephemeral 的 Active Tools 清单合法地
    # 含 ExitPlanMode 工具名,不属于 的规则段范畴。
    assert payload["system"], "system 块不应为空"
    core_text = payload["system"][0]
    assert "金融预测分析师" in core_text, "自定义 system.main 应生效"
    assert "## Plan Mode" not in core_text, (
        f"plan-mode=off 后不得残留 Plan Mode 段: {core_text[:500]}"
    )
    assert "ExitPlanMode" not in core_text, "Plan Mode 指引应整体消失"


def test_未关闭时Plan_Mode段仍在(serve_session, mock_script) -> None:
    """向后兼容:不写覆盖 → Plan Mode 规则段保持既有行为。"""
    require_binary()
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        payload = _request_echo_payload(events)
    core_text = payload["system"][0]
    assert "## Plan Mode" in core_text, "默认行为不变:Plan Mode 段仍在 core 块"


# ── mock 脚本配置错误显性化 ────────────────────────────────────────────


def test_mock脚本全部行解析失败_报_INVALID_REQUEST(
    serve_session, mock_script, tmp_path
) -> None:
    """脚本文件存在但全部行字段名写错 → 首次模型调用显式发
    code=INVALID_REQUEST 的 error 事件(不再静默回退固定回复)。"""
    require_binary()
    bad = tmp_path / "bad-script.jsonl"
    bad.write_text('{"tool":"echo","arguments":{"a":1}}\n')
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(bad)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        errors = _errors_of(events, s.all_events())
        assert errors, "全部行解析失败必须报错"
        err = errors[0]["msg"]
        assert err["code"] == "INVALID_REQUEST", f"字段级断言: {err}"
        assert "全部无法解析" in err["message"], err


def test_mock脚本路径不存在_报_INVALID_REQUEST(serve_session, tmp_path) -> None:
    """ZHUYE_MOCK_SCRIPT 指向不存在的路径 → 同样显式报错。"""
    require_binary()
    missing = tmp_path / "missing.jsonl"
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(missing)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        errors = _errors_of(events, s.all_events())
        assert errors, "路径不存在必须报错"
        assert errors[0]["msg"]["code"] == "INVALID_REQUEST", errors[0]["msg"]
        assert "读取失败" in errors[0]["msg"]["message"], errors[0]["msg"]


# ── mock 默认回复可辨识 ────────────────────────────────────────────────


def test_无脚本时默认回复自带mock标记(serve_session) -> None:
    """未配置脚本 → 回退文案必须自带「mock 固定回复」标记,
    与真实回复可区分(离线测试不再假绿)。"""
    require_binary()
    with serve_session(env={"ZHUYE_MODEL": "mock"}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
        text = _agent_text(events)
    assert "mock 固定回复" in text, f"默认回复应带可辨识标记: {text}"


# `ZHUYE_MODEL` 的 provider 前缀剥离落在 `zhuye-config` crate 单测:
# `strip_known_provider_prefix`(前缀剥离矩阵)与 `resolve_model_inner`
# 注入 env 的组合断言(见该 crate 的测试)。
