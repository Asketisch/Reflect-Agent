#!/usr/bin/env bash
# scripts/release_sdk.sh —— SDK 双仓库(PyPI / npm)发布辅助脚本。
#
# 子命令:
#   check              版本一致性校验 + 本地构建双包并检查(不发布,零 registry 操作)
#   bump <ver>         把 workspace / Python SDK / TS SDK 四处版本同步为 <ver>(幂等)
#   tag [--yes] <ver>  校验通过后打 annotated tag v<ver> 并 push —— 正式发布动作!
#
# ⚠️ tag 子命令会同时触发两条发布流水线(推 v* tag 即生效):
#   - release.yml      → CLI 二进制(DMG / tar.gz / zip)发布到 GitHub Release
#   - release-sdks.yml → Python SDK 发 PyPI、TS SDK 发 npm
#     (需先完成平台侧 Trusted Publishing 配置,见 docs/release.md)
# 测试阶段请只用 check / bump,不要 tag。
#
# 包名不写死在本脚本:一律从 pyproject.toml / package.json 读取,
# 将来项目改名只需改包清单文件,本脚本无需变动。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY_PROJECT="$ROOT/sdks/python/pyproject.toml"
PY_INIT="$ROOT/sdks/python/zhuye/__init__.py"
TS_PACKAGE="$ROOT/sdks/typescript/package.json"
CARGO_TOML="$ROOT/Cargo.toml"
OUT_DIR="$ROOT/target/sdk-release"    # 构建产物落点(已被 .gitignore 覆盖)

die() { echo "FAIL: $*" >&2; exit 1; }

# ── 四处版本号读取(与流水线 gate 保持同一口径)─────────────────────────
cargo_version() {
  awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version = /{print; exit}' \
    "$CARGO_TOML" | cut -d'"' -f2
}
python_version() {
  # [project] 段的 version 是文件中唯一顶层 version 行
  sed -n 's/^version = "\(.*\)"/\1/p' "$PY_PROJECT" | head -n1
}
py_init_version() {
  # Python 包运行时的 __version__(与 pyproject 双写,必须同步)
  sed -n 's/^__version__ = "\(.*\)"/\1/p' "$PY_INIT" | head -n1
}
ts_version() {
  node -p "require('$TS_PACKAGE').version"
}

# 打印四处版本对照表;不一致时直接退出(仅作展示与门禁,不返回值)
check_consistency() {
  echo "版本对照:"
  echo "  Rust workspace: $(cargo_version)  ($CARGO_TOML)"
  echo "  Python SDK:     $(python_version)  ($PY_PROJECT)"
  echo "  Python __version__: $(py_init_version)  ($PY_INIT)"
  echo "  TS SDK:         $(ts_version)  ($TS_PACKAGE)"
  [ "$(cargo_version)" = "$(python_version)" ] \
    || die "版本不一致 —— 先用 bump <ver> 统一。"
  [ "$(python_version)" = "$(py_init_version)" ] \
    || die "版本不一致(__init__.py __version__)—— 先用 bump <ver> 统一。"
  [ "$(python_version)" = "$(ts_version)" ] \
    || die "版本不一致 —— 先用 bump <ver> 统一。"
}

is_semver() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'
}

# ── check:一致性 + 本地构建双包 + 元数据检查 ───────────────────────────
cmd_check() {
  local ver
  check_consistency
  ver="$(cargo_version)"

  # Python 侧:临时 venv 装 build/twine,不动用户全局环境
  # (homebrew python 受 externally-managed 限制,venv 兼容所有安装方式)
  echo
  echo "── Python SDK:构建 sdist + wheel ──"
  # venv 路须为全局变量:EXIT trap 在函数返回后才执行,local 变量已不可见
  venv="$(mktemp -d)"
  trap 'rm -rf "$venv"' EXIT
  python3 -m venv "$venv"
  "$venv/bin/pip" install --quiet --disable-pip-version-check build twine
  mkdir -p "$OUT_DIR/python"
  rm -f "$OUT_DIR/python"/*
  (cd "$ROOT/sdks/python" && "$venv/bin/python" -m build --outdir "$OUT_DIR/python")
  "$venv/bin/twine" check "$OUT_DIR/python"/*
  echo "产物:$OUT_DIR/python"

  # TS 侧:npm ci 保证与 lock 一致,pack --dry-run 预览发布文件清单(不写文件)
  echo
  echo "── TypeScript SDK:构建 + 打包预览 ──"
  mkdir -p "$OUT_DIR/ts"
  (cd "$ROOT/sdks/typescript" \
    && npm ci --no-fund --no-audit \
    && npm run build \
    && npm pack --dry-run --pack-destination "$OUT_DIR/ts")

  echo
  echo "PASSED: 版本 $ver 四处一致,双包构建与元数据检查全部通过(未发布任何内容)。"
}

# ── bump:四处版本同步写入(幂等)──────────────────────────────────────
cmd_bump() {
  local ver="$1"
  is_semver "$ver" || die "bump 需要合法 semver(如 0.2.0、1.0.0-rc.1),收到:$ver"

  local old_c old_p old_i old_t
  old_c="$(cargo_version)"; old_p="$(python_version)"
  old_i="$(py_init_version)"; old_t="$(ts_version)"
  if [ "$old_c" = "$ver" ] && [ "$old_p" = "$ver" ] && [ "$old_i" = "$ver" ] && [ "$old_t" = "$ver" ]; then
    echo "四处版本已经是 $ver,无需 bump。"
    return 0
  fi

  # 统一用 python3 做精确锚点替换:只动 [workspace.package] 段的 version、
  # pyproject 的 version 行、__init__.py 的 __version__、package.json 的
  # "version" 键,不碰依赖版本
  python3 - "$CARGO_TOML" "$old_c" "$ver" <<'PYEOF'
import pathlib, sys

path, old, new = (pathlib.Path(sys.argv[i]) for i in (1, 2, 3))
lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
inside, done, out = False, False, []
for ln in lines:
    if ln.startswith("["):
        inside = ln.startswith("[workspace.package]")
    if inside and not done and ln.rstrip("\n") == f'version = "{old}"':
        ln = f'version = "{new}"\n'
        done = True
    out.append(ln)
assert done, f'未在 [workspace.package] 段找到 version = "{old}"'
path.write_text("".join(out), encoding="utf-8")
PYEOF
  python3 - "$PY_PROJECT" "$old_p" "$ver" <<'PYEOF'
import pathlib, sys

path, old, new = (pathlib.Path(sys.argv[i]) for i in (1, 2, 3))
old_line, new_line = f'version = "{old}"', f'version = "{new}"'
text = path.read_text(encoding="utf-8")
assert text.count(old_line) == 1, f"pyproject 中 {old_line!r} 不唯一"
path.write_text(text.replace(old_line, new_line), encoding="utf-8")
PYEOF
  python3 - "$PY_INIT" "$old_i" "$ver" <<'PYEOF'
import pathlib, sys

path, old, new = (pathlib.Path(sys.argv[i]) for i in (1, 2, 3))
old_line, new_line = f'__version__ = "{old}"', f'__version__ = "{new}"'
text = path.read_text(encoding="utf-8")
assert text.count(old_line) == 1, f"__init__.py 中 {old_line!r} 不唯一"
path.write_text(text.replace(old_line, new_line), encoding="utf-8")
PYEOF
  python3 - "$TS_PACKAGE" "$old_t" "$ver" <<'PYEOF'
import pathlib, sys

path, old, new = (pathlib.Path(sys.argv[i]) for i in (1, 2, 3))
old_key, new_key = f'"version": "{old}"', f'"version": "{new}"'
text = path.read_text(encoding="utf-8")
assert text.count(old_key) == 1, f"package.json 中 {old_key!r} 不唯一"
path.write_text(text.replace(old_key, new_key), encoding="utf-8")
PYEOF

  # 刷新 Cargo.lock 中 workspace 成员的版本记录(离线即可,无需全量 check)
  cargo metadata --format-version 1 --offline > /dev/null 2>&1 \
    || die "cargo metadata 刷新 lock 失败 —— 请手动执行 cargo check --workspace"

  echo
  check_consistency
  echo "bump 完成:$old_c / $old_p / $old_i / $old_t → $ver(Cargo.lock 已刷新)"
}

# ── tag:正式发布触发器 ────────────────────────────────────────────────
cmd_tag() {
  local yes=false ver branch
  if [ "${1:-}" = "--yes" ]; then yes=true; shift; fi
  ver="${1:-}"
  [ -n "$ver" ] || die "tag 用法:release_sdk.sh tag [--yes] <ver>"
  is_semver "$ver" || die "非法 semver:$ver"

  check_consistency
  local cur
  cur="$(cargo_version)"
  [ "$cur" = "$ver" ] || die "本地版本为 $cur,与待打 tag v$ver 不一致 —— 先 bump $ver 并提交。"

  [ -z "$(git -C "$ROOT" status --porcelain)" ] \
    || die "工作区不干净 —— 发布 tag 前先提交全部改动。"
  branch="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
  [ "$branch" = "main" ] \
    || echo "WARN: 当前分支是 $branch(非 main),通常应从 main 发布。" >&2
  git -C "$ROOT" rev-parse -q --verify "refs/tags/v$ver" > /dev/null \
    && die "tag v$ver 已存在。"

  echo
  echo "⚠️  推送 v$ver 将触发正式发布:"
  echo "   - release.yml      → CLI 二进制发到 GitHub Release(公开)"
  echo "   - release-sdks.yml → Python SDK 发 PyPI、TS SDK 发 npm(公开且不可撤回)"
  if [ "$yes" != true ]; then
    read -r -p "确认推送?输入 yes 继续:" ans
    [ "$ans" = "yes" ] || die "已取消。"
  fi
  git -C "$ROOT" tag -a "v$ver" -m "zhuye v$ver(CLI + Python/TS SDK 统一版本)"
  git -C "$ROOT" push origin "v$ver"
  echo "tag v$ver 已推送,发布流水线已触发(可在 Actions 页查看进度)。"
}

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

case "${1:-}" in
  check) shift; cmd_check "$@" ;;
  bump)  shift; [ -n "${1:-}" ] || usage 1; cmd_bump "$1" ;;
  tag)   shift; cmd_tag "$@" ;;
  -h|--help|help|'') usage ;;
  *) die "未知子命令:$1(见 --help)" ;;
esac
