#!/usr/bin/env bash
# ── Reflect-Agent 跨平台构建脚本 ─────────────────────────────────────
# 编译 `reflect` CLI 二进制,可选打包成平台分发包。
#
# 用法(从仓库根目录执行):
#   scripts/build.sh                              # 编译(release-fast,默认,快)
#   scripts/build.sh --release                    # 完整 release(LTO,体积小,慢)
#   scripts/build.sh --fast                       # 同默认(release-fast,显式)
#   scripts/build.sh --install                    # 编译 + 安装到 /usr/local/bin(需 sudo)
#   scripts/build.sh --dist                       # 编译(release)+ 打包成平台包到 dist/
#   scripts/build.sh --dist --target=<triple>     # 交叉编译 + 打包
#   scripts/build.sh --dry-run                    # 只打印命令不执行
#   scripts/build.sh --help
#
# 或通过 Makefile 调用:`make build` / `make install` / `make dist`
#
# --dist 打包格式(按平台自动选择):
#   macOS    → DMG(scripts/build-dmg.sh,含 Finder 拖拽安装)
#   Linux    → tar.gz(scripts/build-targz.sh)
#   Windows  → zip(scripts/build-zip.sh)
# ────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${SCRIPT_DIR}"
BINARY_NAME="reflect"
PACKAGE="reflect-cli"
INSTALL_DIR="/usr/local/bin"

usage() {
    cat <<EOF
用法: $(basename "$0") [OPTIONS]

选项:
  --release          完整 release profile(LTO,codegen-units=1,体积小)
                     默认: release-fast(无 LTO,增量编译快)
  --fast             显式用 release-fast profile(等同默认)
  --install          编译 + 安装到 ${INSTALL_DIR}/(需 sudo)
  --dist             编译(release)+ 打包成平台分发包到 dist/
  --target=TRIPLE    指定 rustc target(交叉编译,如 x86_64-unknown-linux-gnu)
  --dry-run          只打印命令不实际执行
  -h, --help         显示本帮助

--dist 各平台打包格式:
  macOS    → DMG(Finder 拖拽安装)
  Linux    → tar.gz
  Windows  → zip

示例:
  scripts/build.sh                                  # 快速编译(开发循环)
  scripts/build.sh --release --install              # 正式版 + 全局安装
  scripts/build.sh --dist                           # 打包当前平台分发包
  scripts/build.sh --dist --target=x86_64-unknown-linux-gnu  # 交叉编译 Linux 包
EOF
}

# ── 默认值 ────────────────────────────────────────────────────────────
USE_FULL_RELEASE=false
DO_INSTALL=false
DO_DIST=false
DRY_RUN=false
TARGET_OVERRIDE=""

# ── 参数解析(支持 --flag value 与 --flag=value 两种形式)─────────────
while [[ $# -gt 0 ]]; do
    arg="$1"
    case "$arg" in
        --release)  USE_FULL_RELEASE=true; shift ;;
        --fast)     USE_FULL_RELEASE=false; shift ;;
        --install)  DO_INSTALL=true; shift ;;
        --dist)     DO_DIST=true; shift ;;
        --dry-run)  DRY_RUN=true; shift ;;
        --target)   TARGET_OVERRIDE="${2:?--target requires a value}"; shift 2 ;;
        --target=*) TARGET_OVERRIDE="${arg#--target=}"; shift ;;
        -h|--help)  usage; exit 0 ;;
        *)          echo "未知选项: $1" >&2; usage; exit 1 ;;
    esac
done

# ── 辅助:打印 + 执行(尊重 DRY_RUN)──────────────────────────────────
run() {
    echo "+ $*"
    if ! $DRY_RUN; then
        eval "$@"
    fi
}

# ── OS 检测 ───────────────────────────────────────────────────────────
OS_NAME="$(uname -s)"
case "$OS_NAME" in
    Darwin)         PLATFORM="macos" ;;
    Linux)          PLATFORM="linux" ;;
    MINGW*|MSYS*|CYGWIN*) PLATFORM="windows" ;;
    *)
        echo "不支持的 OS: $OS_NAME" >&2
        echo "Reflect 可在 macOS / Linux / Windows(Git Bash)上构建。" >&2
        exit 1
        ;;
esac

# ── profile 决策 ──────────────────────────────────────────────────────
# --dist 强制用 release(分发要小体积);否则用 release-fast(开发快)
if $DO_DIST; then
    PROFILE="release"
    CARGO_PROFILE_FLAG="--release"
    BIN_SUBDIR="release"
elif $USE_FULL_RELEASE; then
    PROFILE="release"
    CARGO_PROFILE_FLAG="--release"
    BIN_SUBDIR="release"
else
    PROFILE="release-fast"
    CARGO_PROFILE_FLAG="--profile release-fast"
    BIN_SUBDIR="release-fast"
fi

# ── target 参数(交叉编译)────────────────────────────────────────────
TARGET_ARGS=()
TARGET_SUBDIR=""
if [[ -n "$TARGET_OVERRIDE" ]]; then
    TARGET_ARGS=(--target "$TARGET_OVERRIDE")
    TARGET_SUBDIR="${TARGET_OVERRIDE}/"
fi

# 二进制产物路径
BIN_PATH="${REPO_ROOT}/target/${TARGET_SUBDIR}${BIN_SUBDIR}/${BINARY_NAME}"

echo "── Reflect-Agent 构建 ──────────────────────────────────────"
echo "平台:          $PLATFORM ($OS_NAME)"
echo "profile:       $PROFILE"
echo "target:        ${TARGET_OVERRIDE:-<host>}"
echo "dist:          $DO_DIST"
echo "install:       $DO_INSTALL"
echo "dry-run:       $DRY_RUN"
echo "───────────────────────────────────────────────────────────"

# ── 工具链检查 ────────────────────────────────────────────────────────
need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "缺少必需工具: $1" >&2
        exit 1
    }
}
need_cmd cargo
need_cmd rustc

# ── 编译 ──────────────────────────────────────────────────────────────
echo "→ 编译 ${BINARY_NAME} (${PROFILE}) ..."
run "cargo build ${CARGO_PROFILE_FLAG} ${TARGET_ARGS[*]:-} -p ${PACKAGE}"

# ── 安装分支 ──────────────────────────────────────────────────────────
if $DO_INSTALL; then
    if [[ ! -e "${BIN_PATH}" ]]; then
        echo "✗ 编译产物不存在: ${BIN_PATH}" >&2
        exit 1
    fi
    echo "→ 安装到 ${INSTALL_DIR}/${BINARY_NAME} ..."
    if [[ ! -w "${INSTALL_DIR}" ]]; then
        run "sudo install -m 0755 '${BIN_PATH}' '${INSTALL_DIR}/${BINARY_NAME}'"
    else
        run "install -m 0755 '${BIN_PATH}' '${INSTALL_DIR}/${BINARY_NAME}'"
    fi
    if ! $DRY_RUN; then
        echo ""
        echo "✓ 安装成功。运行 '${BINARY_NAME} --help' 查看子命令。"
    fi
    exit 0
fi

# ── 打包分支(--dist)─────────────────────────────────────────────────
if $DO_DIST; then
    if [[ ! -e "${BIN_PATH}" ]]; then
        echo "✗ 编译产物不存在: ${BIN_PATH}" >&2
        exit 1
    fi

    # 打包参数:传 profile + target 给子脚本
    DIST_SCRIPT_ARGS=(--profile "${PROFILE}")
    if [[ -n "$TARGET_OVERRIDE" ]]; then
        DIST_SCRIPT_ARGS+=(--target "${TARGET_OVERRIDE}")
    fi

    case "$PLATFORM" in
        macos)
            echo "→ 打包 DMG(macOS)..."
            run "${REPO_ROOT}/scripts/build-dmg.sh --binary '${BIN_PATH}'"
            ;;
        linux)
            echo "→ 打包 tar.gz(Linux)..."
            run "${REPO_ROOT}/scripts/build-targz.sh ${DIST_SCRIPT_ARGS[*]}"
            ;;
        windows)
            echo "→ 打包 zip(Windows)..."
            run "${REPO_ROOT}/scripts/build-zip.sh ${DIST_SCRIPT_ARGS[*]}"
            ;;
    esac

    if ! $DRY_RUN; then
        echo ""
        echo "── 产物 ────────────────────────────────────────────────────"
        if [[ -d "${REPO_ROOT}/dist" ]]; then
            for entry in "${REPO_ROOT}"/dist/*; do
                [[ -e "$entry" ]] || continue
                size="$(du -h "$entry" 2>/dev/null | cut -f1)"
                printf "  %8s  %s\n" "$size" "$entry"
            done
        else
            echo "WARN: dist/ 目录不存在" >&2
        fi
        echo "───────────────────────────────────────────────────────────"
    fi
    exit 0
fi

# ── 默认:仅编译 ──────────────────────────────────────────────────────
if ! $DRY_RUN; then
    echo "✓ 编译完成: ${BIN_PATH}"
fi
