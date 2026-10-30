"""能力覆盖层 · 技能面:内置 skill 包开关(默认关闭)+ 文件 skill 不受影响。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- SK1 默认(不写 `[active] bundled_skills`)→ 内置 skill 不可加载
  (load_skill 报 `skill not found`,字段级断言 is_error 与文案);
- SK2 `[active] bundled_skills = true` → 内置 skill 可加载,断言正文标记;
- SK3 默认下 workspace 文件 skill(`.zhuye/skills/`)不受开关影响,
  依旧可加载 —— 开关只作用于编译期内置包。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_skills.py -v
"""

from __future__ import annotations

from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    Sandbox,
    first_event,
    msg_types,
    require_binary,
)

pytestmark = pytest.mark.usefixtures("sandbox")

# 断言用的内置 skill 名与正文稳定标记(crates/abilities/zhuye-skills/src/bundled/)。
_BUNDLED_NAME = "code-review"
_BUNDLED_MARKER = "thorough code reviewer"


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    """取 tool_call_end 事件(找不到直接失败,附上已见类型)。"""
    m = first_event(events, "tool_call_end")
    assert m is not None, f"未见 tool_call_end:{msg_types(events)}"
    return m["msg"]


def _tool_text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


def _load_skill(
    serve_session, mock_script, name: str, marker: str, *, expect_error: bool
) -> None:
    """公共驱动:mock 调 `load_skill(name)` → tool_call_end 字段级断言。

    `expect_error=True` 断言 `skill not found: <name>`;False 断言成功且
    输出含 `marker`。两条路径都要求 turn 正常收尾。
    """
    require_binary()
    script = mock_script(
        [
            f'{{"type":"tool_call","name":"load_skill","args":{{"name":"{name}"}}}}',
            '{"type":"text","text":"skill-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": f"加载 {name}"}]},
            timeout=DRAIN_TIMEOUT,
        )
    begin = first_event(evs, "tool_call_begin")
    assert begin is not None, f"未见 tool_call_begin:{msg_types(evs)}"
    assert begin["msg"]["tool_name"] == "load_skill", f"tool_name={begin['msg']['tool_name']!r}"
    end = _tool_end(evs)
    if expect_error:
        assert end["is_error"] is True, f"默认应拒绝内置 skill:{end}"
        assert f"skill not found: {name}" in _tool_text(end), f"文案异常:{_tool_text(end)!r}"
    else:
        assert end["is_error"] is False, f"load_skill 应成功:{end}"
        assert marker in _tool_text(end), f"输出未含正文标记:{_tool_text(end)!r}"
    # 工具结果(无论成败)不应打死 turn。
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_SK1_默认关闭内置skill(serve_session, mock_script) -> None:
    """默认配置 → 内置 skill 不在 catalog,load_skill 报 `skill not found`。

    依据:crates/runtime/zhuye-exec/src/bootstrap.rs(`cfg.active.bundled_skills`
    门控 `merge_bundled`)。框架层默认不注入 15 项编码向内置 skill。
    """
    _load_skill(serve_session, mock_script, _BUNDLED_NAME, "", expect_error=True)


def test_SK2_开关开启后内置skill可加载(serve_session, mock_script, sandbox: Sandbox) -> None:
    """`[active] bundled_skills = true` → 合入内置包,正文可按需激活。

    config 必须在 serve 进程启动前写入(`write_config` 追加到隔离 HOME 的
    config.toml,serve_session 工厂每次调用才拉起进程)。
    """
    sandbox.write_config("[active]\nbundled_skills = true\n")
    _load_skill(
        serve_session, mock_script, _BUNDLED_NAME, _BUNDLED_MARKER, expect_error=False
    )


def test_SK3_文件skill不受开关影响(serve_session, mock_script, sandbox: Sandbox) -> None:
    """默认配置下 workspace 文件 skill 依旧可加载 —— 开关只管内置包。"""
    skill_dir = sandbox.cwd / ".zhuye" / "skills" / "file-skill"
    skill_dir.mkdir(parents=True)
    (skill_dir / "SKILL.md").write_text(
        "---\nname: file-skill\ndescription: workspace file skill\n---\nfile-skill-body-marker\n",
        encoding="utf-8",
    )
    _load_skill(
        serve_session, mock_script, "file-skill", "file-skill-body-marker", expect_error=False
    )
