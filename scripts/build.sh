#!/usr/bin/env bash
# kiro-rs Docker 多架构镜像构建脚本
#
# 用法:
#   ./scripts/build.sh                            # 构建当前架构并推送到 ykn1002/kiro-rs:latest
#   ./scripts/build.sh --no-push                  # 仅本地构建，不推送
#   ./scripts/build.sh -t 2026.3.1               # 指定版本 tag
#   ./scripts/build.sh --multi-arch              # 构建 amd64+arm64 多架构 manifest（需 buildx）
#   ./scripts/build.sh --platform linux/amd64     # 指定单平台
#   ./scripts/build.sh --no-cache                 # 禁用构建缓存
#   ./scripts/build.sh --rustls                   # 改用 rustls 构建（更小，默认 native-tls）
#
# 多架构工作流（分机器构建 + manifest 合并）:
#   # ARM 机器
#   ./scripts/build.sh -t 2026.3.1 --arch-only arm64
#   # AMD64 机器
#   ./scripts/build.sh -t 2026.3.1 --arch-only amd64
#   # 任一机器上合并 manifest
#   ./scripts/build.sh -t 2026.3.1 --manifest-only
#
# 环境变量:
#   IMAGE_REPO   完整镜像名，不含 tag（默认 ykn1002/kiro-rs）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

IMAGE_REPO="${IMAGE_REPO:-ykn1002/kiro-rs}"

TAG="latest"
PUSH=true
NO_CACHE=false
PLATFORM=""
RUSTLS=false
MULTI_ARCH=false
ARCH_ONLY=""
MANIFEST_ONLY=false

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \?//'
    echo
    echo "选项:"
    echo "  -t, --tag TAG         镜像 tag（默认 latest）"
    echo "  -p, --push            构建完成后推送（默认开启）"
    echo "      --no-push         仅构建，不推送"
    echo "      --platform PLAT   docker build --platform（如 linux/amd64）"
    echo "      --multi-arch      使用 buildx 一次构建 amd64+arm64 并推送 manifest"
    echo "      --arch-only ARCH  仅构建指定架构并推送带后缀的 tag（amd64 或 arm64）"
    echo "      --manifest-only   仅创建并推送多架构 manifest（需先用 --arch-only 推送各架构）"
    echo "      --no-cache        禁用 Docker 构建缓存"
    echo "      --rustls          构建时改用 rustls（默认 native-tls）"
    echo "  -h, --help            显示此帮助"
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
        --multi-arch)
            MULTI_ARCH=true
            shift
            ;;
        --arch-only)
            ARCH_ONLY="$2"
            shift 2
            ;;
        --manifest-only)
            MANIFEST_ONLY=true
            shift
            ;;
        --no-cache)
            NO_CACHE=true
            shift
            ;;
        --native-tls)
            # 兼容旧参数，默认已是 native-tls
            shift
            ;;
        --rustls)
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

if ! command -v docker >/dev/null 2>&1; then
    echo "错误: 未找到 docker 命令" >&2
    exit 1
fi

# TLS 构建参数
TLS_BUILD_ARG=""
if [[ "$RUSTLS" == true ]]; then
    TLS_BUILD_ARG="--build-arg ENABLE_NATIVE_TLS=false"
fi

# ============================================================
# 模式 1: --manifest-only — 仅合并已推送的架构镜像为 manifest
# ============================================================
if [[ "$MANIFEST_ONLY" == true ]]; then
    echo "==> 创建多架构 manifest: ${IMAGE_REPO}:${TAG}"

    # 清理可能已存在的本地 manifest
    docker manifest rm "${IMAGE_REPO}:${TAG}" 2>/dev/null || true

    docker manifest create "${IMAGE_REPO}:${TAG}" \
        "${IMAGE_REPO}:${TAG}-amd64" \
        "${IMAGE_REPO}:${TAG}-arm64"
    docker manifest push "${IMAGE_REPO}:${TAG}"
    echo "==> manifest 推送完成: ${IMAGE_REPO}:${TAG}"

    # 如果 tag 不是 latest，额外更新 latest
    if [[ "$TAG" != "latest" ]]; then
        docker manifest rm "${IMAGE_REPO}:latest" 2>/dev/null || true
        docker manifest create "${IMAGE_REPO}:latest" \
            "${IMAGE_REPO}:${TAG}-amd64" \
            "${IMAGE_REPO}:${TAG}-arm64"
        docker manifest push "${IMAGE_REPO}:latest"
        echo "==> manifest 推送完成: ${IMAGE_REPO}:latest"
    fi
    exit 0
fi

# ============================================================
# 模式 2: --multi-arch — 使用 buildx 一次构建多架构并推送
# ============================================================
if [[ "$MULTI_ARCH" == true ]]; then
    echo "==> 使用 buildx 构建多架构镜像: amd64 + arm64"

    BUILDX_ARGS=(
        --platform linux/amd64,linux/arm64
        -f Dockerfile
        -t "${IMAGE_REPO}:${TAG}"
    )

    if [[ "$TAG" != "latest" ]]; then
        BUILDX_ARGS+=(-t "${IMAGE_REPO}:latest")
    fi

    if [[ "$NO_CACHE" == true ]]; then
        BUILDX_ARGS+=(--no-cache)
    fi

    if [[ -n "$TLS_BUILD_ARG" ]]; then
        BUILDX_ARGS+=($TLS_BUILD_ARG)
    fi

    if [[ "$PUSH" == true ]]; then
        BUILDX_ARGS+=(--push)
    fi

    if [[ "$RUSTLS" == true ]]; then
        echo "    TLS: rustls"
    else
        echo "    TLS: native-tls（默认）"
    fi

    docker buildx build "${BUILDX_ARGS[@]}" .
    echo "==> 多架构构建完成"
    exit 0
fi

# ============================================================
# 模式 3: --arch-only ARCH — 构建单架构并推送带后缀的 tag
# ============================================================
if [[ -n "$ARCH_ONLY" ]]; then
    if [[ "$ARCH_ONLY" != "amd64" && "$ARCH_ONLY" != "arm64" ]]; then
        echo "错误: --arch-only 仅支持 amd64 或 arm64" >&2
        exit 1
    fi

    FULL_IMAGE="${IMAGE_REPO}:${TAG}-${ARCH_ONLY}"
    echo "==> 构建单架构镜像: ${FULL_IMAGE} (linux/${ARCH_ONLY})"

    BUILD_ARGS=(
        -f Dockerfile
        --platform "linux/${ARCH_ONLY}"
        -t "${FULL_IMAGE}"
    )

    if [[ "$NO_CACHE" == true ]]; then
        BUILD_ARGS+=(--no-cache)
    fi

    if [[ -n "$TLS_BUILD_ARG" ]]; then
        BUILD_ARGS+=($TLS_BUILD_ARG)
    fi

    if [[ "$RUSTLS" == true ]]; then
        echo "    TLS: rustls"
    else
        echo "    TLS: native-tls（默认）"
    fi

    docker build "${BUILD_ARGS[@]}" .
    echo "==> 构建完成: ${FULL_IMAGE}"

    if [[ "$PUSH" == true ]]; then
        echo "==> 推送 ${FULL_IMAGE}"
        docker push "${FULL_IMAGE}"
        echo "==> 推送完成"
    fi
    exit 0
fi

# ============================================================
# 模式 4: 默认 — 构建当前架构并推送（兼容旧行为）
# ============================================================
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

if [[ -n "$TLS_BUILD_ARG" ]]; then
    BUILD_ARGS+=($TLS_BUILD_ARG)
fi

echo "==> 构建镜像 ${FULL_IMAGE}"
if [[ -n "$PLATFORM" ]]; then
    echo "    平台: ${PLATFORM}"
fi
if [[ "$RUSTLS" == true ]]; then
    echo "    TLS: rustls"
else
    echo "    TLS: native-tls（默认）"
fi

docker build "${BUILD_ARGS[@]}" .

echo "==> 构建完成: ${FULL_IMAGE}"

if [[ "$PUSH" == true ]]; then
    echo "==> 推送 ${FULL_IMAGE}"
    docker push "${FULL_IMAGE}"
    echo "==> 推送完成"
fi
