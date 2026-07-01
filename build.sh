#!/usr/bin/env bash
# kiro-rs Docker 镜像构建脚本
#
# 用法:
#   ./build.sh                          # 构建并推送到 ykn1002/kiro-rs:latest
#   ./build.sh --no-push                  # 仅本地构建，不推送
#   ./build.sh -t 2026.3.1              # 指定版本 tag
#   ./build.sh --platform linux/amd64     # 指定平台（默认当前架构）
#   ./build.sh --no-cache                 # 禁用构建缓存
#
# 环境变量:
#   IMAGE_REPO   完整镜像名，不含 tag（默认 ykn1002/kiro-rs）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

IMAGE_REPO="${IMAGE_REPO:-ykn1002/kiro-rs}"

TAG="latest"
PUSH=true
NO_CACHE=false
PLATFORM=""

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \?//'
    echo
    echo "选项:"
    echo "  -t, --tag TAG       镜像 tag（默认 latest）"
    echo "  -p, --push          构建完成后推送（默认开启）"
    echo "      --no-push       仅构建，不推送"
    echo "      --platform PLAT docker build --platform（如 linux/amd64,linux/arm64）"
    echo "      --no-cache      禁用 Docker 构建缓存"
    echo "  -h, --help          显示此帮助"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -t|--tag)
            TAG="$2"
            shift 2
            ;;
        -p|--push)
            PUSH=true
            shift
            ;;
        --no-push)
            PUSH=false
            shift
            ;;
        --platform)
            PLATFORM="$2"
            shift 2
            ;;
        --no-cache)
            NO_CACHE=true
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

if ! command -v docker >/dev/null 2>&1; then
    echo "错误: 未找到 docker 命令" >&2
    exit 1
fi

FULL_IMAGE="${IMAGE_REPO}:${TAG}"

BUILD_ARGS=(
    -f Dockerfile
    -t "${FULL_IMAGE}"
    --label "org.opencontainers.image.source=https://github.com/${IMAGE_REPO%%/*}/${IMAGE_REPO##*/}"
)

if [[ -n "$PLATFORM" ]]; then
    BUILD_ARGS+=(--platform "$PLATFORM")
fi

if [[ "$NO_CACHE" == true ]]; then
    BUILD_ARGS+=(--no-cache)
fi

echo "==> 构建镜像 ${FULL_IMAGE}"
if [[ -n "$PLATFORM" ]]; then
    echo "    平台: ${PLATFORM}"
fi

docker build "${BUILD_ARGS[@]}" .

echo "==> 构建完成: ${FULL_IMAGE}"

if [[ "$PUSH" == true ]]; then
    echo "==> 推送 ${FULL_IMAGE}"
    docker push "${FULL_IMAGE}"
    echo "==> 推送完成"
fi
