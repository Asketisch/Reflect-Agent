#!/usr/bin/env bash
# ── scripts/build-targz.sh ───────────────────────────────────────────
# 把 `reflect` CLI 二进制打包成 tar.gz 压缩包,用于 Linux / macOS 分发。
#
# 产物结构(解压后顶层目录名 = 产物名去掉后缀):
#   reflect-<version>-<triple>/
#   ├── reflect            ← 可执行二进制
#   ├── LICENSE
#   └── README.txt         ← 安装与使用说明
#
# 与 build-dmg.sh 互补:DMG 提供 macOS 图形化拖拽安装体验;
# tar.gz 提供跨平台、CI 友好、脚本可解析的通用打包格式。
#
# 用法:
#   scripts/build-targz.sh                         # 用 target/release/reflect
#   scripts/build-targz.sh --binary <path>         # 指定预构建二进制
#   scripts/build-targz.sh --profile release-fast  # 指定 profile(默认 release)
#   scripts/build-targz.sh --target <triple>       # 交叉编译产物路径推算
#   scripts/build-targz.sh --help
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

# 确定源二进制路径 ────────────────────────────────────────────────────
if [[ -n "${CUSTOM_BINARY}" ]]; then
    SOURCE_BIN="${CUSTOM_BINARY}"
else
    # target 目标子目录:--target 时嵌套一层 target/<triple>/
    TARGET_SUBDIR=""
    if [[ -n "${TARGET_TRIPLE}" ]]; then
        TARGET_SUBDIR="${TARGET_TRIPLE}/"
    fi
    SOURCE_BIN="${REPO_ROOT}/target/${TARGET_SUBDIR}${PROFILE}/${APP_NAME}"
fi

if [[ ! -x "${SOURCE_BIN}" ]]; then
    echo "✗ 二进制不存在或不可执行: ${SOURCE_BIN}" >&2
    echo "  请先运行 scripts/build.sh,或传 --binary <path>。" >&2
    exit 1
fi

# 确定 target triple:优先用参数,否则从二进制推断
if [[ -z "${TARGET_TRIPLE}" ]]; then
    ARCH="$(uname -m)"
    OS="$(uname -s)"
    case "$OS" in
        Darwin) TARGET_TRIPLE="${ARCH}-apple-darwin" ;;
        Linux)  TARGET_TRIPLE="${ARCH}-unknown-linux-gnu" ;;
        *)      TARGET_TRIPLE="${ARCH}-unknown" ;;
    esac
fi

# 路径 ────────────────────────────────────────────────────────────────
DIST_DIR="${REPO_ROOT}/dist"
PKG_NAME="${APP_NAME}-${VERSION}-${TARGET_TRIPLE}"
STAGING_DIR="$(mktemp -d -t reflect-targz-staging)"
ARCHIVE_PATH="${DIST_DIR}/${PKG_NAME}.tar.gz"

echo "→ 打包 tar.gz: ${PKG_NAME}"

mkdir -p "${DIST_DIR}"

# ── 准备 staging 内容 ────────────────────────────────────────────────
PKG_DIR="${STAGING_DIR}/${PKG_NAME}"
mkdir -p "${PKG_DIR}"

# (a) 二进制本体
cp "${SOURCE_BIN}" "${PKG_DIR}/${APP_NAME}"
chmod +x "${PKG_DIR}/${APP_NAME}"

# (b) 许可证文件(LICENSE)
if [[ -f "${REPO_ROOT}/LICENSE" ]]; then
    cp "${REPO_ROOT}/LICENSE" "${PKG_DIR}/LICENSE"
fi

# (c) README.txt —— 安装与使用说明
cat > "${PKG_DIR}/README.txt" <<README_EOF
Reflect ${VERSION}  (${TARGET_TRIPLE})
=========================================================

Reflect 是一个 Rust 编写的 agent 运行时(默认进入 TUI)。

【安装】解压后把二进制放到 PATH:
    tar xzf ${PKG_NAME}.tar.gz
    sudo install -m 0755 ${PKG_NAME}/${APP_NAME} /usr/local/bin/${APP_NAME}
    # 或装到无需 sudo 的位置:
    install -m 0755 ${PKG_NAME}/${APP_NAME} ~/.local/bin/${APP_NAME}

【验证】
    ${APP_NAME} --version
    ${APP_NAME} --help
    ${APP_NAME}              # 进入交互式 TUI

【首次配置】登录 LLM provider:
    ${APP_NAME} login --provider anthropic
    ${APP_NAME} login --provider openai
    或直接编辑 ~/.reflect/config.toml

【平台】本包仅适用于 ${TARGET_TRIPLE}。

详见 https://github.com/CNB/Reflect
README_EOF

# ── 打包 ─────────────────────────────────────────────────────────────
# 在 staging 目录下打包,使顶层目录为 ${PKG_NAME}/
echo "→ 创建 ${ARCHIVE_PATH} ..."
tar -C "${STAGING_DIR}" -czf "${ARCHIVE_PATH}" "${PKG_NAME}"

# ── 清理 + 汇总 ──────────────────────────────────────────────────────
rm -rf "${STAGING_DIR}"

ARCHIVE_SIZE_HUMAN="$(du -h "${ARCHIVE_PATH}" | awk '{print $1}')"
BIN_SIZE_HUMAN="$(du -h "${SOURCE_BIN}" | awk '{print $1}')"

echo ""
echo "✓ tar.gz 已生成。"
echo "  路径:   ${ARCHIVE_PATH}"
echo "  大小:   ${ARCHIVE_SIZE_HUMAN}  (二进制: ${BIN_SIZE_HUMAN})"
