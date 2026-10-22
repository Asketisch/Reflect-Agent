"""实模型演练层 · 公共设施(模型来源、能力探测、隔离环境)。

## 这一层测什么

能力覆盖层(`tests/functional/`)用 mock provider,断言确定性行为;
本层用**真实模型**跑同一条引擎链路,断言的是**协议形状与运行时行为**
(事件序、终态、工具调用成对、失败分类),**不断言具体回答文本** ——
真实模型的输出不确定,逐字断言只会制造假失败。

## 模型从哪来

**从 provider 接口实时获取**,不写死模型 id:

1. `REFLECT_LIVE_MODELS` 显式给了逗号分隔列表 → 用它;
2. 否则调 `reflect models list --free-only --json` 拿清单 →
   逐个做**能力探测**(带 `tools` 的最小请求;provider 拒绝 tools 的模型
   标为「跳过」而不是「失败」)→ 取前 N 个。

模型集合由上游决定,随时可能增删 —— 写死必然腐烂。

## 跑法

```bash
REFLECT_LIVE=1 ./tests/run_all.sh live
```

不设 `REFLECT_LIVE=1` 时全部用例 skip,退出码 0,**不发起任何网络请求**。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

import pytest

# 复用能力覆盖层的原始 wire 设施(不复制一份)。
_TESTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(_TESTS_DIR / "functional"))

from wire import REFLECT_BIN, Sandbox  # noqa: E402

# 实模型首字可能很慢(免费池实测 >30s,思考阶段可静默 60~70s),
# 因此 live 层的事件等待预算比 mock 层宽得多。
LIVE_DRAIN_TIMEOUT = float(os.environ.get("REFLECT_LIVE_DRAIN_TIMEOUT", "180"))

# 免费池默认车道:Kilo 公共网关(免 Key、无客户端指纹门)。
DEFAULT_BASE_URL = os.environ.get(
    "REFLECT_LIVE_BASE_URL", "https://api.kilo.ai/api/gateway"
)
# 免 Key 端点:默认空字符串(配合「空 api_key 不发 Authorization 头」)。
DEFAULT_API_KEY = os.environ.get("REFLECT_LIVE_API_KEY", "")

# 模型矩阵规模(仅在未显式指定模型时生效)。
MODEL_COUNT = int(os.environ.get("REFLECT_LIVE_MODEL_COUNT", "3"))

# 单个模型的能力探测超时(秒)—— 探测失败即视为「该模型不可用」。
PROBE_TIMEOUT = float(os.environ.get("REFLECT_LIVE_PROBE_TIMEOUT", "90"))


def live_enabled() -> bool:
    """是否启用实模型层(默认关闭)。"""
    return os.environ.get("REFLECT_LIVE", "").strip() not in ("", "0", "false")


# ── 模型清单(从 provider 接口获取)──────────────────────────────────────────


def fetch_free_models(base_url: str, api_key: str) -> list[dict]:
    """拉取 provider 的免费模型清单。

    走 `reflect models list --free-only --json` —— 免费判定规则只有一份
    实现(在 Rust 侧),不在 Python 里重写一遍。
    """
    home = Path(os.environ.get("REFLECT_LIVE_TMP_HOME", "/tmp")) / "reflect-live-models"
    home.mkdir(parents=True, exist_ok=True)
    (home / ".reflect").mkdir(exist_ok=True)
    (home / ".reflect/config.toml").write_text(
        "[active]\nprovider = \"openai\"\n\n"
        "[openai]\n"
        f"api_key = \"{api_key}\"\n"
        f"base_url = \"{base_url}\"\n"
        "timeout_secs = 300\n"
        "model = \"free\"\n\n"
        "[[openai.credentials]]\n"
        "label = \"live\"\n"
        f"api_key = \"{api_key}\"\n"
        f"base_url = \"{base_url}\"\n"
        "model = \"free\"\n"
    )
    env = dict(os.environ)
    env["HOME"] = str(home)
    proc = subprocess.run(
        [str(REFLECT_BIN), "models", "list", "--free-only", "--json"],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"reflect models list 失败(rc={proc.returncode}):{proc.stderr.strip()}"
        )
    payload = json.loads(proc.stdout)
    return payload.get("models", [])


def probe_tool_support(base_url: str, api_key: str, model: str) -> tuple[bool, str]:
    """能力探测:该模型是否支持 tools。

    返回 `(supported, reason)`。**关键区分**:provider 拒绝 tools(400)与
    上游限流/故障(429/5xx/网络)是两回事 —— 前者说明「该模型不支持
    tools」,后者只说明「现在问不了」。把 429 当成「不支持 tools」会
    误杀本来可用的模型。
    """
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": "Say OK"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "probe_tool",
                        "description": "probe",
                        "parameters": {
                            "type": "object",
                            "properties": {"x": {"type": "string"}},
                        },
                    },
                }
            ],
            "max_tokens": 64,
            "stream": False,
        }
    ).encode()
    req = urllib.request.Request(
        f"{base_url.rstrip('/')}/v1/chat/completions",
        data=body,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    if api_key:
        req.add_header("Authorization", f"Bearer {api_key}")
    try:
        with urllib.request.urlopen(req, timeout=PROBE_TIMEOUT) as resp:
            return resp.status == 200, f"http_{resp.status}"
    except urllib.error.HTTPError as exc:
        detail = ""
        try:
            detail = exc.read().decode("utf-8", "replace")[:160]
        except Exception:  # noqa: BLE001 - 诊断信息尽力而为
            pass
        if exc.code == 429 or exc.code >= 500:
            # 限流 / 上游故障 —— 「暂时问不了」,不是「不支持 tools」。
            return True, f"unavailable_http_{exc.code}"
        return False, f"http_{exc.code}:{detail}"
    except Exception as exc:  # noqa: BLE001 - 网络异常:同样按「暂时问不了」处理
        return True, f"unavailable:{exc}"


def resolve_models() -> list[tuple[str, bool, str]]:
    """解析本次要跑的模型矩阵。

    返回 `[(model_id, supports_tools, reason)]`。
    """
    explicit = os.environ.get("REFLECT_LIVE_MODELS", "").strip()
    if explicit:
        candidates = [m.strip() for m in explicit.split(",") if m.strip()]
        probed: list[tuple[str, bool, str]] = []
        for model in candidates:
            ok, reason = probe_tool_support(DEFAULT_BASE_URL, DEFAULT_API_KEY, model)
            probed.append((model, ok, reason))
        return probed

    entries = fetch_free_models(DEFAULT_BASE_URL, DEFAULT_API_KEY)
    if not entries:
        raise RuntimeError(
            "provider 清单里没有免费模型;检查 REFLECT_LIVE_BASE_URL / 出口地区"
        )
    # 清单按 id 排序后取前 N 个 —— 确定性可复现。
    entries = sorted(entries, key=lambda e: e.get("id", ""))
    picked: list[tuple[str, bool, str]] = []
    for entry in entries:
        if len(picked) >= MODEL_COUNT:
            break
        model = entry.get("id")
        if not model:
            continue
        ok, reason = probe_tool_support(DEFAULT_BASE_URL, DEFAULT_API_KEY, model)
        picked.append((model, ok, reason))
    return picked


# ── fixtures ───────────────────────────────────────────────────────────────


@pytest.fixture(scope="session")
def live_matrix() -> list[tuple[str, bool, str]]:
    """模型矩阵(session 级)。未启用时整个 session 跳过。"""
    if not live_enabled():
        pytest.skip("实模型层未启用(设 REFLECT_LIVE=1 开启)")
    try:
        matrix = resolve_models()
    except Exception as exc:  # noqa: BLE001 - 清单拉取失败 = 环境不可用
        pytest.skip(f"无法获取模型清单:{exc}")
    if not matrix:
        pytest.skip("没有可用模型")
    return matrix


@pytest.fixture()
def live_sandbox(tmp_path: Path, live_matrix) -> Sandbox:
    """隔离 HOME + cwd,并写入指向真实 provider 的配置。

    配置里用**具体模型 id**(由清单选出)而不是 `free` —— live 层要能精确
    断言「本轮跑的是哪个模型」。
    """
    home = tmp_path / "home"
    cwd = tmp_path / "cwd"
    (home / ".reflect").mkdir(parents=True)
    cwd.mkdir(parents=True)
    model = live_matrix[0][0]
    (home / ".reflect/config.toml").write_text(
        "# 实模型演练层配置:关闭全部内置 hook(只验引擎行为,不验 hook 计时)\n"
        "[hooks]\nenabled = []\n\n"
        "[active]\nprovider = \"openai\"\n\n"
        "[openai]\n"
        f"api_key = \"{DEFAULT_API_KEY}\"\n"
        f"base_url = \"{DEFAULT_BASE_URL}\"\n"
        "timeout_secs = 300\n"
        f"model = \"{model}\"\n\n"
        "[[openai.credentials]]\n"
        "label = \"live\"\n"
        f"api_key = \"{DEFAULT_API_KEY}\"\n"
        f"base_url = \"{DEFAULT_BASE_URL}\"\n"
        f"model = \"{model}\"\n"
    )
    return Sandbox(home=home, cwd=cwd)


@pytest.fixture()
def live_session(live_sandbox):
    """起 `reflect serve` 会话(真实模型)。"""
    from wire import ServeSession

    created: list[ServeSession] = []

    def _make(args: list[str] | None = None, env: dict[str, str] | None = None):
        s = ServeSession(
            live_sandbox,
            args=args,
            env=env,
            timeout=LIVE_DRAIN_TIMEOUT,
        )
        created.append(s)
        return s

    yield _make
    for s in created:
        try:
            s.close(timeout=5.0)
        except Exception:  # noqa: BLE001 - 收尾尽力而为
            pass


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    """把各阶段的 report 挂到 item 上(诊断 fixture 需要知道是否失败)。"""
    outcome = yield
    rep = outcome.get_result()
    setattr(item, f"rep_{rep.when}", rep)


@pytest.fixture(autouse=True)
def _live_diagnostics(request, live_sandbox):
    """用例失败时打印配置与 serve stderr —— 排障的第一手材料。"""
    yield
    rep = getattr(request.node, "rep_call", None)
    if rep is not None and rep.failed:
        cfg = live_sandbox.home / ".reflect/config.toml"
        print("\n──── 实模型层失败诊断 ────")
        print(f"[config] {cfg}")
        try:
            print(cfg.read_text())
        except Exception:  # noqa: BLE001 - 诊断尽力而为
            pass
        stderr = live_sandbox.cwd / "serve.stderr"
        if stderr.exists():
            print("[serve stderr] 末尾 30 行:")
            print("\n".join(stderr.read_text().splitlines()[-30:]))
        print("─────────────────────────")
