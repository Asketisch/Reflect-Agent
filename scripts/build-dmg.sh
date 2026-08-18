#!/usr/bin/env bash
# ── scripts/build-dmg.sh ──────────────────────────────────────────────
# 把 `reflect` CLI 打包成 macOS DMG 安装镜像,用于全局安装后使用。
#
# DMG 打开后的画面(Finder 图标视图):
#
#   ┌───────────────────────────────────────┐
#   │  Reflect <version>                    │
#   │                                       │
#   │   ┌─────────────┐   ┌─────────────┐  │
#   │   │  reflect    │   │ Applications │  │  ← 拖入即装
#   │   └─────────────┘   └─────────────┘  │
#   │                                       │
#   │   install.sh   uninstall.sh   README  │  ← 命令行安装脚本
#   └───────────────────────────────────────┘
#
# 安装方式(任选其一):
#   1. 图形界面:把 `reflect` 拖到 `Applications`(装到 /Applications/reflect.app)
#   2. 命令行(推荐,装到全局 PATH):
#        sh /Volumes/Reflect\\ <version>/install.sh
#      → 装到 /usr/local/bin/reflect
#      → 加 --local 改装到 ~/.local/bin(无需 sudo)
#
# 流程(hdiutil 自带,无需 create-dmg 等第三方工具):
#   1) 准备 staging 目录(二进制 + 脚本 + Applications 软链 + README + LICENSE)
#   2) hdiutil create → UDRW(可读写)临时镜像
#   3) open 挂载 → AppleScript 设置 Finder 图标布局(写 .DS_Store)
#   4) detach → hdiutil convert → UDZO 压缩只读镜像(分发格式)
#
# 用法:
#   scripts/build-dmg.sh                 # 用 target/release/reflect
#   scripts/build-dmg.sh --rebuild       # 先 cargo build --release
#   scripts/build-dmg.sh --binary <path> # 指定一个预构建的 reflect 二进制
#   scripts/build-dmg.sh --open          # 打包后自动挂载预览
#
# ────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# 元数据 ──────────────────────────────────────────────────────────────
APP_NAME="reflect"
FINAL_VOL_NAME_TEMPLATE="Reflect {}"   # {} 占位 version
ARCH="$(uname -m)"
TARGET_TRIPLE="${ARCH}-apple-darwin"

# 版本号:优先取二进制自己报告的,失败则回退到 Cargo.toml。
extract_version() {
    local bin="$1"
    local vers
    if vers="$("${bin}" --version 2>/dev/null | awk '{print $2}')" && [[ -n "${vers}" ]]; then
        printf '%s' "${vers}"
    else
        awk -F'=' '
            /^\[workspace\.package\]/ { in_pkg=1; next }
            /^\[/ { in_pkg=0 }
            in_pkg && $1 ~ /^version[[:space:]]*$/ { gsub(/[ "]/,"",$2); print $2; exit }
        ' "${REPO_ROOT}/Cargo.toml"
    fi
}

# 参数解析 ────────────────────────────────────────────────────────────
DO_REBUILD=false
DO_OPEN=false
CUSTOM_BINARY=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rebuild)        DO_REBUILD=true;   shift ;;
        --open)           DO_OPEN=true;      shift ;;
        --binary)         CUSTOM_BINARY="${2:?--binary needs a path}"; shift 2 ;;
        --binary=*)       CUSTOM_BINARY="${1#*=}";                       shift ;;
        -h|--help)
            sed -n '2,42p' "$0"; exit 0 ;;
        *)  echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# 确定源二进制 ────────────────────────────────────────────────────────
if [[ -n "${CUSTOM_BINARY}" ]]; then
    SOURCE_BIN="${CUSTOM_BINARY}"
elif [[ "${DO_REBUILD}" == true ]]; then
    SOURCE_BIN="${REPO_ROOT}/target/release/${APP_NAME}"
    echo "→ cargo build --release -p reflect-cli ..."
    (cd "${REPO_ROOT}" && cargo build --release -p reflect-cli)
else
    SOURCE_BIN="${REPO_ROOT}/target/release/${APP_NAME}"
fi

if [[ ! -x "${SOURCE_BIN}" ]]; then
    echo "✗ Binary not found or not executable: ${SOURCE_BIN}" >&2
    echo "  Run with --rebuild, or pass --binary <path>."     >&2
    exit 1
fi
if ! file "${SOURCE_BIN}" | grep -q 'Mach-O'; then
    echo "✗ Not a Mach-O binary: ${SOURCE_BIN}" >&2
    exit 1
fi

VERSION="$(extract_version "${SOURCE_BIN}")"
if [[ -z "${VERSION}" ]]; then
    echo "✗ Could not determine version (neither binary --version nor Cargo.toml)." >&2
    exit 1
fi

# 路径 ────────────────────────────────────────────────────────────────
FINAL_VOL_NAME="Reflect ${VERSION}"
STAGING_DIR="$(mktemp -d -t reflect-dmg-staging)"
RW_DMG="$(mktemp -t reflect-rw -u).dmg"
DIST_DIR="${REPO_ROOT}/dist"
DMG_NAME="${APP_NAME}-${VERSION}-${TARGET_TRIPLE}.dmg"
DMG_PATH="${DIST_DIR}/${DMG_NAME}"
VOL_MOUNT="/Volumes/${FINAL_VOL_NAME}"

# 清理逻辑:无论成功失败,把临时目录 / RW dmg / 挂载点都拆掉。
cleanup() {
    set +e
    if mount | grep -q "on ${VOL_MOUNT} "; then
        hdiutil detach "${VOL_MOUNT}" -force -quiet
    elif [[ -d "${VOL_MOUNT}" ]]; then
        hdiutil detach "${VOL_MOUNT}" -force -quiet
    fi
    [[ -f "${RW_DMG}" ]] && rm -f "${RW_DMG}"
    rm -rf "${STAGING_DIR}"
}
trap cleanup EXIT

echo "→ Staging DMG contents at ${STAGING_DIR} ..."

# ── 1. 准备 staging 内容 ────────────────────────────────────────────

# (a) 二进制本体(放最外层,可直接 ./reflect 运行 / 拖入 Applications)
cp "${SOURCE_BIN}" "${STAGING_DIR}/${APP_NAME}"
chmod +x "${STAGING_DIR}/${APP_NAME}"

# (b) install.sh:全局安装到 /usr/local/bin(或 ~/.local/bin)
cat > "${STAGING_DIR}/install.sh" <<'INSTALL_EOF'
#!/usr/bin/env bash
# Reflect 全局安装脚本(随 DMG 分发)。
# 默认装到 /usr/local/bin(需要 sudo);用 --local 装到 ~/.local/bin(无需 sudo)。
set -euo pipefail

INSTALL_DIR="/usr/local/bin"
BIN_NAME="reflect"
SUDO=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --local)
            INSTALL_DIR="${HOME}/.local/bin"
            shift ;;
        --install-dir)
            INSTALL_DIR="${2:?}"
            shift 2 ;;
        -y|--yes) shift ;;
        -h|--help)
            echo "Usage: install.sh [--local|--install-dir <dir>] [-y]"
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# 找二进制:DMG 挂载点 / 脚本所在目录
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="${SCRIPT_DIR}/${BIN_NAME}"
if [[ ! -x "${SRC}" ]]; then
    echo "✗ ${BIN_NAME} not found next to install.sh (${SCRIPT_DIR})" >&2
    exit 1
fi

# /usr/local/bin 通常需要 root;~/.local/bin 不需要。
if [[ "${INSTALL_DIR}" == /usr/local/bin ]] && [[ ! -w "${INSTALL_DIR}" ]]; then
    SUDO="sudo"
fi
mkdir -p "${INSTALL_DIR}" 2>/dev/null || ${SUDO} mkdir -p "${INSTALL_DIR}"

echo "→ Installing ${BIN_NAME} → ${INSTALL_DIR}/${BIN_NAME}"
if [[ -n "${SUDO}" ]]; then
    ${SUDO} install -m 0755 "${SRC}" "${INSTALL_DIR}/${BIN_NAME}"
else
    install -m 0755 "${SRC}" "${INSTALL_DIR}/${BIN_NAME}"
fi

# ~/.local/bin 需要确保在 PATH 里
if [[ "${INSTALL_DIR}" == "${HOME}/.local/bin" ]]; then
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            for rc in "${HOME}/.zshrc" "${HOME}/.bashrc"; do
                [[ -f "${rc}" ]] || continue
                if ! grep -q "export PATH=\"${INSTALL_DIR}:\$PATH\"" "${rc}"; then
                    printf '\n# Added by Reflect installer\nexport PATH="%s:$PATH"\n' "${INSTALL_DIR}" >> "${rc}"
                    echo "→ Added ${INSTALL_DIR} to PATH in $(basename "${rc}")"
                fi
            done
            export PATH="${INSTALL_DIR}:${PATH}"
            ;;
    esac
fi

# 健康检查
if command -v "${BIN_NAME}" >/dev/null 2>&1; then
    echo ""
    echo "✓ Installed: $(${BIN_NAME} --version)"
    echo "  Location:  $(command -v "${BIN_NAME}")"
    echo ""
    echo "  Run '${BIN_NAME}' for the interactive TUI."
    echo "  Run '${BIN_NAME} --help' to see all subcommands."
else
    echo ""
    echo "✓ Installed to ${INSTALL_DIR}/${BIN_NAME}"
    echo "  (start a new shell or open a new terminal tab to get it on PATH)"
fi
INSTALL_EOF
chmod +x "${STAGING_DIR}/install.sh"

# (c) 卸载脚本(uninstall.sh)
cat > "${STAGING_DIR}/uninstall.sh" <<'UNINSTALL_EOF'
#!/usr/bin/env bash
# 卸载全局安装的 Reflect CLI。
set -euo pipefail
BIN_NAME="reflect"
REMOVED=0

for dir in /usr/local/bin "${HOME}/.local/bin"; do
    target="${dir}/${BIN_NAME}"
    if [[ -f "${target}" ]]; then
        if [[ -w "${dir}" ]]; then
            rm -f "${target}"
        else
            sudo rm -f "${target}"
        fi
        echo "→ Removed ${target}"
        REMOVED=1
    fi
done

# 顺手提示用户数据目录(默认不删,避免误删配置 / 会话历史)
echo ""
echo "Note: user data remains in ~/.reflect/ (config, sessions, plugins)."
echo "      To wipe it: rm -rf ~/.reflect"
[[ "${REMOVED}" == 1 ]] || echo "(nothing to remove in /usr/local/bin or ~/.local/bin)"
UNINSTALL_EOF
chmod +x "${STAGING_DIR}/uninstall.sh"

# (d) Applications 软链(拖拽安装目标)—— 用 -srcfolder 一并打进 DMG
ln -sf /Applications "${STAGING_DIR}/Applications"

# (e) README.txt — DMG 打开后用户第一眼看到的说明
cat > "${STAGING_DIR}/README.txt" <<README_EOF
Reflect ${VERSION}  (${ARCH}-apple-darwin)
=========================================================

Reflect 是一个 Rust 编写的 agent 运行时(默认进入 TUI)。

【安装】两种方式任选其一:

  方式 A(推荐,命令行全局安装):
    打开「终端」,执行:
      sh /Volumes/Reflect\\ ${VERSION}/install.sh
    默认装到 /usr/local/bin/reflect(需要 sudo 输密码)。
    想装到不需要 sudo 的位置:
      sh /Volumes/Reflect\\ ${VERSION}/install.sh --local

  方式 B(图形界面):
    把 reflect 拖到右侧的 Applications 文件夹。

【验证】安装后新开一个终端:
    reflect --version
    reflect --help
    reflect              # 进入交互式 TUI

【首次配置】登录 LLM provider:
    reflect login --provider anthropic
    reflect login --provider openai
    或直接编辑 ~/.reflect/config.toml

【卸载】
    sh /Volumes/Reflect\\ ${VERSION}/uninstall.sh

【平台】本包仅适用于 macOS ${ARCH}(Apple Silicon)。
        Intel Mac 请使用 x86_64 包;Linux 请使用 tar.gz 包。

详见 https://github.com/CNB/Reflect
README_EOF

# (f) 许可证文件(LICENSE)
if [[ -f "${REPO_ROOT}/LICENSE" ]]; then
    cp "${REPO_ROOT}/LICENSE" "${STAGING_DIR}/LICENSE"
fi

# ── 2. 创建可读写 DMG ───────────────────────────────────────────────
mkdir -p "${DIST_DIR}"

echo "→ Creating read-write DMG ..."
# UDRW = 读写,后面才能挂载改 .DS_Store;最终再 convert 成 UDZO 压缩格式。
# -size 必须显式给:srcfolder 装进去后,UDRW 默认大小会等于文件总大小,没余量
# 写 .DS_Store / .fseventsd。给 staging 实际占用 + 20MiB 余量。
STAGING_SIZE_KB="$(du -sk "${STAGING_DIR}" | awk '{print $1}')"
RW_SIZE_MB=$(( (STAGING_SIZE_KB + 20480 + 1023) / 1024 ))

hdiutil create -srcfolder "${STAGING_DIR}" \
    -volname "${FINAL_VOL_NAME}" \
    -fs HFS+ \
    -format UDRW \
    -size "${RW_SIZE_MB}M" \
    -ov \
    "${RW_DMG}" >/dev/null

# ── 3. 挂载(用 open,通过 Finder/Desktop 协议,绕开 hdiutil attach 在某些
#       受限环境下的 EPERM)→ AppleScript 设图标布局 → 写入 .DS_Store ──
echo "→ Mounting for layout (Finder window + icon positions) ..."
open "${RW_DMG}"

# 等挂载点出现(最多 ~10s)
for _ in $(seq 1 20); do
    [[ -d "${VOL_MOUNT}" ]] && break
    sleep 0.5
done
if [[ ! -d "${VOL_MOUNT}" ]]; then
    echo "✗ Failed to mount RW dmg at ${VOL_MOUNT}" >&2
    echo "  (you may be in a restricted environment that blocks disk mounts)" >&2
    exit 1
fi

# AppleScript:打开 Finder 窗口、设为图标视图、定位主要图标。
# .DS_Store 由 Finder 写到卷根,记录窗口大小 / 视图 / 图标坐标。
# 即使 AppleScript 失败(比如 headless 环境),也只影响「图标位置」,
# DMG 内容与 install.sh 仍完全可用,所以这里不 abort 整个构建。
if ! osascript 2>/dev/null <<OSA ; then
    echo "  (warning: Finder layout skipped — DMG still usable, icons may auto-arrange)"
tell application "Finder"
    set volDisk to disk "${FINAL_VOL_NAME}"
    set win to make new Finder window to volDisk
    set current view of win to icon view
    set toolbar visible of win to false
    set statusbar visible of win to false
    set bounds of win to {100, 100, 640, 440}
    set opts to icon view options of win
    set arrangement of opts to not arranged
    set icon size of opts to 80
    try
        set position of item "reflect"       of volDisk to {120, 160}
    end try
    try
        set position of item "Applications"  of volDisk to {420, 160}
    end try
    try
        set position of item "install.sh"    of volDisk to {120, 320}
    end try
    try
        set position of item "uninstall.sh"  of volDisk to {270, 320}
    end try
    try
        set position of item "README.txt"    of volDisk to {420, 320}
    end try
    close win
end tell
OSA
    true
fi

# 让 .DS_Store 落盘。关键点:必须先让 Finder 正常 eject,它才会把窗口
# 视图/图标坐标 flush 到卷根的 .DS_Store。直接 hdiutil detach -force 会
# 绕过 Finder 的 flush 循环,.DS_Store 写进去也会丢。这里先 Finder eject,
# 再 hdiutil detach 兜底(force,以防 Finder 已弹)。
sync
osascript -e "tell application \"Finder\" to eject disk \"${FINAL_VOL_NAME}\"" 2>/dev/null || true
# 等 Finder 真正弹完(最多 ~10s)
for _ in $(seq 1 20); do
    [[ -d "${VOL_MOUNT}" ]] || break
    sleep 0.5
done
# 兜底:若还挂着,强制卸载
[[ -d "${VOL_MOUNT}" ]] && hdiutil detach "${VOL_MOUNT}" -force -quiet

# ── 4. 转换成只读、UDZO 压缩的最终 DMG(适合分发) ───────────────────
echo "→ Converting to compressed read-only DMG ..."
rm -f "${DMG_PATH}"
hdiutil convert "${RW_DMG}" \
    -format UDZO \
    -imagekey zlib-level=9 \
    -o "${DMG_PATH}" >/dev/null

# ── 5. 校验 ────────────────────────────────────────────────────────
echo "→ Verifying DMG ..."
if ! hdiutil verify "${DMG_PATH}" >/dev/null; then
    echo "✗ DMG verification failed: ${DMG_PATH}" >&2
    exit 1
fi

DMG_SIZE_HUMAN="$(du -h "${DMG_PATH}" | awk '{print $1}')"
BIN_SIZE_HUMAN="$(du -h "${SOURCE_BIN}" | awk '{print $1}')"

echo ""
echo "✓ DMG created."
echo "  Path:   ${DMG_PATH}"
echo "  Size:   ${DMG_SIZE_HUMAN}  (binary: ${BIN_SIZE_HUMAN})"
echo "  Volume: ${FINAL_VOL_NAME}"
echo ""
echo "  Install:"
echo "    open  '${DMG_PATH}'"
echo "    sh    '/Volumes/Reflect ${VERSION}/install.sh'"

if [[ "${DO_OPEN}" == true ]]; then
    open "${DMG_PATH}"
fi
