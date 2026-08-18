#!/usr/bin/env bash
# ── scripts/build-zip.sh ─────────────────────────────────────────────
# 把 `reflect` CLI 二进制打包成 zip 压缩包,用于 Windows 分发。
#
# 产物结构(解压后顶层目录名 = 产物名去掉后缀):
#   reflect-<version>-<triple>/
#   ├── reflect.exe        ← 可执行二进制
#   ├── LICENSE
#   └── README.txt
#
# 优先使用系统 zip 命令;Windows Git Bash 无 zip 时回退到 PowerShell
# 的 Compress-Archive(Git Bash 自带 powershell.exe)。
#
# 用法:
#   scripts/build-zip.sh                          # 用 target/release/reflect.exe
#   scripts/build-zip.sh --binary <path>          # 指定预构建二进制
#   scripts/build-zip.sh --profile release-fast   # 指定 profile(默认 release)
#   scripts/build-zip.sh --target <triple>        # 交叉编译产物路径推算
#   scripts/build-zip.sh --help
# ────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# 元数据 ──────────────────────────────────────────────────────────────
APP_NAME="reflect"
PROFILE="release"
CUSTOM_BINARY=""
TARGET_TRIPLE=""

# 参数解析 ────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)   CUSTOM_BINARY="${2:?--binary needs a path}"; shift 2 ;;
        --binary=*) CUSTOM_BINARY="${1#*=}";                        shift ;;
        --profile)  PROFILE="${2:?--profile needs a value}";       shift 2 ;;
        --profile=*) PROFILE="${1#*=}";                             shift ;;
        --target)  TARGET_TRIPLE="${2:?--target needs a value}";  shift 2 ;;
        --target=*) TARGET_TRIPLE="${1#*=}";                       shift ;;
        -h|--help)  sed -n '2,22p' "$0"; exit 0 ;;
        *)  echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# 版本号:从 Cargo.toml [workspace.package] 提取
extract_version() {
    awk -F'=' '
        /^\[workspace\.package\]/ { in_pkg=1; next }
        /^\[/ { in_pkg=0 }
        in_pkg && $1 ~ /^version[[:space:]]*$/ { gsub(/[ "]/,"",$2); print $2; exit }
    ' "${REPO_ROOT}/Cargo.toml"
}

VERSION="$(extract_version)"
if [[ -z "${VERSION}" ]]; then
    echo "✗ 无法从 Cargo.toml 提取版本号" >&2
    exit 1
fi

# 确定源二进制路径(Windows 产物带 .exe 后缀)────────────────────────
if [[ -n "${CUSTOM_BINARY}" ]]; then
    SOURCE_BIN="${CUSTOM_BINARY}"
else
    TARGET_SUBDIR=""
    if [[ -n "${TARGET_TRIPLE}" ]]; then
        TARGET_SUBDIR="${TARGET_TRIPLE}/"
    fi
    # 优先找 .exe(Windows),找不到回退到无后缀(macOS/Linux 上交叉编译 Windows 目标)
    SOURCE_BIN="${REPO_ROOT}/target/${TARGET_SUBDIR}${PROFILE}/${APP_NAME}.exe"
    if [[ ! -e "${SOURCE_BIN}" ]]; then
        SOURCE_BIN="${REPO_ROOT}/target/${TARGET_SUBDIR}${PROFILE}/${APP_NAME}"
    fi
fi

if [[ ! -e "${SOURCE_BIN}" ]]; then
    echo "✗ 二进制不存在: ${SOURCE_BIN}" >&2
    echo "  请先运行 scripts/build.sh --target=<triple>,或传 --binary <path>。" >&2
    exit 1
fi

# 确定 target triple
if [[ -z "${TARGET_TRIPLE}" ]]; then
    OS="$(uname -s)"
    ARCH="$(uname -m)"
    case "$OS" in
        MINGW*|MSYS*|CYGWIN*) TARGET_TRIPLE="${ARCH}-pc-windows-msvc" ;;
        *)                    TARGET_TRIPLE="${ARCH}-pc-windows-msvc" ;;
    esac
fi

# 路径 ────────────────────────────────────────────────────────────────
DIST_DIR="${REPO_ROOT}/dist"
PKG_NAME="${APP_NAME}-${VERSION}-${TARGET_TRIPLE}"
STAGING_DIR="$(mktemp -d -t reflect-zip-staging)"
ARCHIVE_PATH="${DIST_DIR}/${PKG_NAME}.zip"

echo "→ 打包 zip: ${PKG_NAME}"

mkdir -p "${DIST_DIR}"

# ── 准备 staging 内容 ────────────────────────────────────────────────
PKG_DIR="${STAGING_DIR}/${PKG_NAME}"
mkdir -p "${PKG_DIR}"

# (a) 二进制本体(保留原文件名,可能是 reflect.exe 或 reflect)
BIN_BASENAME="$(basename "${SOURCE_BIN}")"
cp "${SOURCE_BIN}" "${PKG_DIR}/${BIN_BASENAME}"

# (b) 许可证文件(LICENSE)
if [[ -f "${REPO_ROOT}/LICENSE" ]]; then
    cp "${REPO_ROOT}/LICENSE" "${PKG_DIR}/LICENSE"
fi

# (c) 说明文件(README.txt)
cat > "${PKG_DIR}/README.txt" <<README_EOF
Reflect ${VERSION}  (${TARGET_TRIPLE})
=========================================================

Reflect 是一个 Rust 编写的 agent 运行时(默认进入 TUI)。

【安装】解压后把 reflect.exe 放到 PATH 目录(如 C:\Users\<你>\bin\):
    1. 右键 zip → 解压到当前位置
    2. 把 reflect.exe 移到某个 PATH 目录,或把解压目录加入 PATH

【验证】打开「命令提示符」或「PowerShell」:
    reflect --version
    reflect --help
    reflect              # 进入交互式 TUI

【首次配置】登录 LLM provider:
    reflect login --provider anthropic
    reflect login --provider openai
    或直接编辑 %USERPROFILE%\.reflect\config.toml

【平台】本包仅适用于 ${TARGET_TRIPLE}。

详见 https://github.com/CNB/Reflect
README_EOF

# ── 打包 ─────────────────────────────────────────────────────────────
echo "→ 创建 ${ARCHIVE_PATH} ..."

# 优先用系统 zip 命令;Windows Git Bash 可能没有,回退到 PowerShell
if command -v zip >/dev/null 2>&1; then
    (cd "${STAGING_DIR}" && zip -r -q "${ARCHIVE_PATH}" "${PKG_NAME}")
else
    # 回退:PowerShell Compress-Archive(Git Bash / Windows 自带 powershell)
    PS_STAGING="$(cygpath -w "${STAGING_DIR}" 2>/dev/null || echo "${STAGING_DIR}")"
    PS_ARCHIVE="$(cygpath -w "${ARCHIVE_PATH}" 2>/dev/null || echo "${ARCHIVE_PATH}")"
    powershell.exe -NoProfile -Command \
        "Compress-Archive -Path '${PS_STAGING}\\${PKG_NAME}' -DestinationPath '${PS_ARCHIVE}' -Force"
fi

# ── 清理 + 汇总 ──────────────────────────────────────────────────────
rm -rf "${STAGING_DIR}"

ARCHIVE_SIZE_HUMAN="$(du -h "${ARCHIVE_PATH}" | awk '{print $1}')"

echo ""
echo "✓ zip 已生成。"
echo "  路径:   ${ARCHIVE_PATH}"
echo "  大小:   ${ARCHIVE_SIZE_HUMAN}"
