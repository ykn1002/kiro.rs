#!/usr/bin/env bash
# kiro-rs 桌面版（Tauri）打包脚本
#
# 用法:
#   ./build-desktop.sh                    # 打当前架构（Apple Silicon 上即 arm64）
#   ./build-desktop.sh --intel            # 打 Intel (x86_64) 版
#   ./build-desktop.sh --universal        # 打通用二进制（arm64+x86_64 合一）
#   ./build-desktop.sh --bundles app      # 只出 .app，不打 dmg（更快）
#   ./build-desktop.sh --skip-frontend    # 跳过前端构建（admin-ui/dist 已是最新时）
#   ./build-desktop.sh --universal --native-tls       # 改用 native-tls（默认 rustls）
#
# 说明:
#   - 前端 admin-ui 会先构建（rust-embed 需要 admin-ui/dist），除非 --skip-frontend。
#   - 桌面版默认用 rustls，避免 native-tls 在打包环境的动态链接问题。
#   - Intel / universal 为交叉编译，需要 x86_64 target；脚本会自动尝试安装。
#   - 产物路径:
#       arm64:     src-tauri/target/release/bundle/{macos,dmg}/
#       其它 target: src-tauri/target/<triple>/release/bundle/{macos,dmg}/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# 默认参数
ARCH="host"           # host | intel | universal
BUNDLES="app,dmg"
SKIP_FRONTEND=false
RUSTLS=true

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \?//'
    echo
    echo "选项:"
    echo "      --intel           打 Intel (x86_64) 版"
    echo "      --universal       打通用二进制（arm64+x86_64）"
    echo "      --bundles LIST    bundle 类型（默认 app,dmg；可 app 或 dmg）"
    echo "      --skip-frontend   跳过 admin-ui 前端构建"
    echo "      --native-tls      改用 native-tls（默认 rustls）"
    echo "  -h, --help            显示此帮助"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --intel)
            ARCH="intel"
            shift
            ;;
        --universal)
            ARCH="universal"
            shift
            ;;
        --bundles)
            BUNDLES="$2"
            shift 2
            ;;
        --skip-frontend)
            SKIP_FRONTEND=true
            shift
            ;;
        --native-tls)
            RUSTLS=false
            shift
            ;;
        --rustls)
            # 兼容显式指定，默认已是 rustls
            RUSTLS=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "未知参数: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# ---- 依赖检查 ----
if ! command -v pnpm >/dev/null 2>&1; then
    echo "错误: 未找到 pnpm 命令" >&2
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "错误: 未找到 cargo 命令" >&2
    exit 1
fi
if ! cargo tauri --version >/dev/null 2>&1; then
    echo "错误: 未找到 tauri CLI，请先安装：cargo install tauri-cli --version '^2' --locked" >&2
    exit 1
fi

# ---- 目标三元组与 rust target 安装 ----
TARGET_ARG=()      # 传给 cargo tauri build 的 --target
TARGET_TRIPLE=""   # 用于定位产物目录
case "$ARCH" in
    host)
        echo "==> 目标架构: 当前机器（本机）"
        ;;
    intel)
        TARGET_TRIPLE="x86_64-apple-darwin"
        TARGET_ARG=(--target "$TARGET_TRIPLE")
        echo "==> 目标架构: Intel ($TARGET_TRIPLE)"
        ;;
    universal)
        TARGET_TRIPLE="universal-apple-darwin"
        TARGET_ARG=(--target "$TARGET_TRIPLE")
        echo "==> 目标架构: 通用二进制 ($TARGET_TRIPLE)"
        ;;
esac

# 交叉编译需要对应 rust target（universal 需要 x86_64 与 aarch64 两个）
ensure_target() {
    local t="$1"
    if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
        echo "==> 安装 rust target: $t"
        rustup target add "$t"
    fi
}
case "$ARCH" in
    intel)
        ensure_target "x86_64-apple-darwin"
        ;;
    universal)
        ensure_target "x86_64-apple-darwin"
        ensure_target "aarch64-apple-darwin"
        ;;
esac

# ---- 前端构建（rust-embed 依赖 admin-ui/dist）----
if [[ "$SKIP_FRONTEND" == true ]]; then
    echo "==> 跳过前端构建（--skip-frontend）"
    if [[ ! -f admin-ui/dist/index.html ]]; then
        echo "错误: admin-ui/dist 不存在，不能跳过前端构建" >&2
        exit 1
    fi
else
    echo "==> 构建前端 admin-ui"
    ( cd admin-ui && pnpm install && pnpm build )
fi

# ---- 清理可能残留的 dmg 挂载卷（避免 hdiutil 报卷宗占用）----
if [[ "$BUNDLES" == *dmg* ]]; then
    for v in /Volumes/kiro-rs*; do
        [[ -e "$v" ]] && hdiutil detach "$v" -force >/dev/null 2>&1 || true
    done
fi

# ---- TLS feature ----
FEATURE_ARGS=(--no-default-features --features rustls)
if [[ "$RUSTLS" == true ]]; then
    echo "==> TLS: rustls（默认）"
else
    FEATURE_ARGS=(--no-default-features --features native-tls)
    echo "==> TLS: native-tls"
fi

# ---- 打包 ----
echo "==> 打包桌面版（bundles: ${BUNDLES}）"
# 注意: bash 3.2（macOS 自带）在 set -u 下展开空数组会报 unbound，故用 ${arr[@]:-} 兜底
cargo tauri build ${TARGET_ARG[@]:-} --bundles "$BUNDLES" -- "${FEATURE_ARGS[@]}"

# ---- 产物路径提示 ----
if [[ -n "$TARGET_TRIPLE" ]]; then
    BUNDLE_DIR="src-tauri/target/${TARGET_TRIPLE}/release/bundle"
else
    BUNDLE_DIR="src-tauri/target/release/bundle"
fi

echo
echo "==> 打包完成，产物位于:"
[[ -d "$BUNDLE_DIR/macos" ]] && ls -1d "$BUNDLE_DIR/macos/"*.app 2>/dev/null || true
[[ -d "$BUNDLE_DIR/dmg" ]] && ls -1d "$BUNDLE_DIR/dmg/"*.dmg 2>/dev/null || true
